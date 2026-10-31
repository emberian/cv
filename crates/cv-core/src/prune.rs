//! `cv prune` — a *custom, lossless compaction* of a Claude Code session into a **new, resumable**
//! session. Instead of handing a full context window to the summarizer (which rewrites your history
//! into a lossy paragraph), prune snips the **bulky old tool payloads** — large file reads, command
//! logs, base64 screenshots — out of the conversation and into a sidecar, leaving a small `[PRUNED
//! id=…]` marker in their place. Your prompts and the chronological flow are preserved; the most
//! recent turns are left untouched so the model stays sharp on what it's doing right now.
//!
//! The approach is adapted from the validated `flatten-mcp` flattener: we operate on the **raw
//! JSONL lines**, never on our IR — so every Claude-specific field survives a round-trip and the
//! result resumes cleanly with `claude --resume <new-id>`. An untouched line is byte-identical to
//! its source except for the session id ([`crate::restamp_line`]'s byte surgery); a line with a
//! snipped payload goes through serde_json, which may normalize key order and whitespace, with the
//! JSON **values** — and the payloads stashed in the sidecar — preserved verbatim. What we add on
//! top of the flattener: a fresh session id stamped across every line (so it's a genuinely new
//! session, not an in-place rewrite), a copy of the session's sidecar resources (`subagents/`,
//! `workflows/`) under the new id, and a **keep-last** window so only *old* payloads are snipped.
//!
//! Two payload sites are handled, exactly as Claude Code records them:
//!  1. `tool_result` blocks inside a user message's `message.content` — the bytes the model re-reads
//!     every turn (this is what actually costs context tokens).
//!  2. the top-level `toolUseResult` mirror Claude keeps per result line — disk-only (the model never
//!     sees it), but snipping it shrinks the file and our parse.
//!
//! Lines with `isSidechain: true` are sub-agent turns whose token usage belongs to a *different*
//! context window: they ride along as content (kept, dropped, or snipped by position) but are never
//! counted as conversational turns, never contribute usage to `--window` sizing or `--revive`
//! arithmetic, and are never chosen as the re-root of a windowed tail.
//!
//! Claude Code's synthetic assistant notices (`model: "<synthetic>"` — "Prompt is too long" and
//! friends) are not turns either: they ride along as records but never count toward `--keep-last`,
//! `--range` indices or `--window` sizing. Model-visible `attachment` records (those with
//! `rendered[]`) DO count toward byte estimates — that text is in the prompt.

use crate::harness::claude::record::Facts;
use crate::harness::claude::skills::{self, SkillsPolicy, SkillsReport};
use crate::harness::store::{
    bytes::{ByteSource, ByteStage},
    jsonl::ranges::{self, Range},
};
use crate::json::{ReadInput, Validation};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Marker prefix stamped where a payload used to be; carries enough to retrieve the original. The
/// sidecar's public contract: `cv cat` keys retrieval on the id after it.
pub const MARKER_PREFIX: &str = "[PRUNED id=";
/// Suffix distinguishing a flattened `toolUseResult` mirror entry from its `content` sibling.
const TUR_SUFFIX: &str = "#tur";
/// Prune estimates over raw records (it never parses to the IR), so it walks `message.content`
/// itself, pricing each field with the shared [`crate::ir::TokenKind`] rates, [`IMAGE_TOKENS`] per
/// image and [`BLOCK_TOKENS`] per block — the numbers the IR-side estimator produces.
use crate::ir::{TokenKind, BLOCK_TOKENS, IMAGE_TOKENS};

/// Knobs for [`prune_session`].
#[derive(Debug, Clone)]
pub struct PruneOptions {
    /// Snip a tool payload only if it is larger than this many bytes.
    pub min_size: usize,
    /// Keep the last N conversational turns' payloads verbatim (recent context stays sharp).
    pub keep_last: usize,
    /// Hard-drop payloads (no sidecar, irreversible) instead of stashing them for retrieval.
    pub drop: bool,
    /// Also omit old assistant **thinking** blocks (extended reasoning): on resume you rarely need
    /// the verbatim old reasoning — the conclusions are in the assistant's text. Omits every old
    /// block regardless of `min_size` (a signature-only block is ~700 bytes on disk but hundreds of
    /// tokens on the wire), leaving no marker; a record left empty is removed and its children
    /// relinked. Lossless (stashed in the sidecar like any other payload). Off by default — it
    /// changes more than tool output.
    pub thinking: bool,
    /// Explicit new session id; otherwise a fresh UUIDv7.
    pub new_id: Option<String>,
    /// Also copy the session's `subagents/`/`workflows/` resource dir under the new id. Off by
    /// default — `claude --resume` doesn't need it, and for big sessions it can be hundreds of MB /
    /// thousands of files. Turn on if you want cv's forest features to work on the pruned session.
    pub copy_resources: bool,
    /// After pruning, rewrite trailing `message.usage` records using an estimated post-prune
    /// prompt size. Claude Code's resume gate reads recorded usage before sending a new request
    /// (including `iterations` when present). Recorded-usage deltas and byte estimates inform the
    /// replacement; it is not provider-measured new usage and does not verify resume acceptance.
    /// Off by default at the core level. Only the new transcript's metadata is changed.
    pub revive: bool,
    /// Sliding window: keep only the NEWEST conversational turns whose content sums to **≤ this many
    /// tokens**, dropping older turns entirely (lossy — but the source session is never touched, so it
    /// stays the full backup). The kept tail is re-rooted into a standalone resumable session. If even
    /// the single newest turn exceeds the budget, that one turn is kept anyway and a warning is
    /// reported (see [`PruneResult::warnings`]) — resume may still hit the context wall. Snapping the
    /// tail back to its opening user prompt can also add back a fraction of one turn.
    pub window: Option<u64>,
    /// Explicit message subrange `[start, end)` (turn indices) to keep, dropping everything outside
    /// it. `end = None` ⇒ through the last turn. Like `window` but selected by index, not budget.
    pub keep_range: Option<(usize, Option<usize>)>,
    /// `--declassify`: snip conversational message TEXT whose security-token density is high, so a
    /// resumed session does not carry classifier-tripping PROSE into a fresh (e.g. Fable) seat's context.
    /// Lossless (stashed in the sidecar like any other payload). Complements tool-payload/`--thinking`
    /// snipping — the trigger content lives largely in the conversational prose those passes leave
    /// verbatim. Off by default. Unlike the tool/thinking passes, this does NOT respect `keep_last`: a
    /// trigger span anywhere in the loaded context downgrades the seat, so recent security prose is
    /// snipped too (still retrievable). Benign turns (< `declassify_min_hits` terms) are untouched.
    pub declassify: bool,
    /// Terms (lowercase) counted for `--declassify` density — ALWAYS caller-supplied (no built-in
    /// list; empty ⇒ `--declassify` is a no-op). Matched case-insensitively as substrings.
    pub declassify_tokens: Vec<String>,
    /// Snip a message iff it holds at least this many DISTINCT `declassify_tokens` (default 2).
    pub declassify_min_hits: usize,
    /// Which skills the new session restores at its next compaction or resume (see
    /// [`crate::harness::claude::skills`]). `Keep` (the default) copies the records as they are.
    pub skills_policy: SkillsPolicy,
    /// `--plan`: the caller's judgment of each tool result and which attachment kinds to drop
    /// ([`Plan`]). Replaces the `min_size`/`keep_last` payload snipping: only planned results
    /// change. `--drop-thinking`, `--declassify`, windows and revive compose as without it.
    pub plan: Option<Plan>,
    /// Compute and report without writing anything.
    pub dry_run: bool,
    /// Replace an existing `<new-id>.jsonl` (and its sidecar) by atomic rename. Without it, an
    /// existing name is refused with [`crate::publish::Exists`].
    pub force: bool,
}

impl Default for PruneOptions {
    fn default() -> Self {
        PruneOptions {
            min_size: 2048,
            keep_last: 25,
            drop: false,
            thinking: false,
            new_id: None,
            copy_resources: false,
            revive: false,
            window: None,
            keep_range: None,
            declassify: false,
            declassify_tokens: Vec::new(), // token-agnostic: caller supplies the terms (no baked-in list)
            declassify_min_hits: 2,
            skills_policy: SkillsPolicy::Keep,
            plan: None,
            dry_run: false,
            force: false,
        }
    }
}

/// What a prune did (or, in `dry_run`, would do). Serializes as the `cv prune --json` report (the
/// renamed keys are that report's spelling); the three `--revive` details are folded by the CLI
/// into its `revived` object.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PruneResult {
    pub source_id: String,
    /// Native parent identity when the selected source is a child transcript.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub new_id: String,
    pub new_path: PathBuf,
    pub sidecar_path: Option<PathBuf>,
    pub copied_resources: Option<PathBuf>,
    #[serde(rename = "snipped_payloads")]
    pub pruned_count: usize,
    pub image_blocks: usize,
    #[serde(rename = "before_bytes")]
    pub original_size: u64,
    #[serde(rename = "after_bytes")]
    pub new_size: u64,
    #[serde(rename = "tokens_freed")]
    pub est_context_tokens_saved: u64,
    /// `--revive`: how many `message.usage` records were rewritten with estimates.
    #[serde(skip)]
    pub usage_rewritten: usize,
    /// `--window`/`--range`: conversational turns dropped from the head/tail by the selection (0 if
    /// neither was used). The kept turns form the new session; the source keeps everything.
    pub dropped_turns: usize,
    /// `--window`: kept-tail size inferred from recorded prompt-usage deltas. The legacy field
    /// name does not imply measured usage after pruning or predict a provider's resumed request.
    /// `None` for `--range`, or when byte estimates substitute for absent usage records.
    pub window_real_tokens: Option<u64>,
    /// `--revive`: estimated prompt size written to usage metadata; not a new provider measurement.
    /// `None` when revival is disabled or no rewrite is needed.
    #[serde(skip)]
    pub revive_tokens: Option<u64>,
    /// `--revive`: the stale figure it replaced — the largest pre-revive usage total in the loaded window.
    #[serde(skip)]
    pub revive_old_tokens: Option<u64>,
    /// Data-safety warnings (e.g. a `--window` tail that exceeds its budget because a single turn is
    /// bigger than the whole budget). Also echoed to stderr.
    pub warnings: Vec<String>,
    /// What `skills_policy` dropped from the new session's restored skills.
    #[serde(skip_serializing_if = "SkillsReport::is_keep")]
    pub skills: SkillsReport,
    /// `--plan`: what the plan changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanReport>,
    pub dry_run: bool,
}

impl PruneResult {
    pub fn bytes_saved(&self) -> u64 {
        self.original_size.saturating_sub(self.new_size)
    }
}

/// One stashed payload, written as a line of `<new-id>.flat.jsonl` and read back by [`retrieve`].
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct SidecarEntry<S = String, I = Value, C = Value> {
    id: S,
    slot: S, // "content" | "toolUseResult"
    name: S,
    input: I,
    content: C, // the ORIGINAL value, verbatim — lossless
    size: usize,
    line_count: usize,
    kind: S, // "text" | "image" | "mixed"
}
pub(crate) type BorrowedSidecarEntry<'a> = SidecarEntry<&'a str, &'a Value, &'a str>;
impl<'a> BorrowedSidecarEntry<'a> {
    pub(crate) fn text(id: &'a str, name: &'a str, input: &'a Value, content: &'a str) -> Self {
        Self {
            id,
            slot: "content",
            name,
            input,
            content,
            size: content.len(),
            line_count: content.lines().count(),
            kind: "text",
        }
    }
}

/// One sidecar line in the `<stem>.flat.jsonl` format [`retrieve`] (and `cv cat`) reads: the stash
/// format is shared with `cv distill`, which elides whole tool outputs the same way prune snips them.
pub fn sidecar_line(id: &str, name: &str, input: &Value, content: &str) -> String {
    serde_json::to_string(&BorrowedSidecarEntry::text(id, name, input, content)).unwrap_or_default()
}

/// What prune decides one record on: the codec's typed [`Facts`] plus prune's own pricing.
#[derive(Debug, Clone, Default)]
struct Decision {
    facts: Facts,
    /// The record's estimated prompt contribution (only under `--window`/`--revive`); after a
    /// snip, the estimate of what is left.
    est: Est,
    /// A tool payload (`toolUseResult` or a `tool_result` body) larger than `--min-size`.
    payload: bool,
    thinking: bool,
    /// `--plan`: a tool result here gets a planned précis or excerpt.
    planned: bool,
    /// `--plan`: tool results here the plan keeps verbatim.
    plan_keep: usize,
    /// `--plan`: this attachment's kind is dropped (the kind).
    drop_attachment: Option<String>,
}

impl Decision {
    /// Sub-agent (Task tool) lines carry `isSidechain: true`; their usage belongs to the
    /// sub-agent's own context window, not the main thread's.
    fn sidechain(&self) -> bool {
        self.facts.sidechain
    }
    /// A main-thread conversational turn: `Some(is_user)`; `None` for bookkeeping, sidechain
    /// lines and Claude Code's synthetic notices, which never shift turn indices or eat
    /// `--keep-last` budget.
    fn turn_kind(&self) -> Option<bool> {
        self.facts.turn_kind()
    }
    fn is_main_turn(&self) -> bool {
        self.turn_kind().is_some()
    }
    /// The prompt size the record carries: `_cv_orig_ctx` (the real count a prior revive
    /// stashed) over the live usage, which revive may have overwritten with a pinned figure.
    fn recorded(&self) -> u64 {
        self.facts.orig_ctx.or(self.facts.usage_in).unwrap_or(0)
    }
}

/// Session-level records written once (or rarely) that a windowed tail must not lose — the label
/// and identity facts Claude Code's picker and loader read: the legacy `summary`, both titles, the
/// tag, agent name, a fork's relocation, the running cost, the ATIS latch and a continuation
/// pointer. The LAST occurrence of each survives wherever it sits; per-prompt records (`mode`,
/// `last-prompt`, …) recur constantly and need no help.
const SINGLETON_RECORDS: &[&str] = &[
    "summary",
    "ai-title",
    "custom-title",
    "tag",
    "agent-name",
    "relocated",
    "cost-state",
    "atis-latch",
    "continued-in",
];

/// Size a `--window` tail using recorded Claude prompt-usage deltas. Returns
/// `(turn to keep from, inferred tail size, overshoot)`, or `None` if usage is absent (then the
/// caller uses byte estimates). If the newest turn alone exceeds the budget, keep it and warn.
///
/// Within one compaction segment, subtract the preceding prompt count from the end count to
/// estimate the tail contribution. This assumes comparable, growing prompts with a shared prefix;
/// it does not measure the standalone request after rewriting or guarantee its size on resume.
/// Older compaction segments cannot be treated as a clean shared prefix. Sidechain usage belongs
/// to a different prompt and is excluded from this main-thread calculation.
fn usage_window_cutoff(parsed: &[Option<Decision>], budget: u64) -> Option<(usize, u64, bool)> {
    // Per main-thread turn that carries a usage record: (turn index, cumulative usage, starts a new
    // compaction segment). Cumulative usage is monotonic WITHIN a segment but resets across a
    // compaction boundary, so we can't subtract across the whole session — we work in per-turn deltas.
    let mut ti = 0usize;
    let mut recs: Vec<(usize, u64, bool)> = Vec::new();
    let mut boundary_pending = false;
    for v in parsed.iter().flatten() {
        if v.sidechain() {
            continue;
        }
        if v.is_main_turn() {
            let tot = v.recorded();
            if tot > 0 {
                recs.push((ti, tot, boundary_pending));
                boundary_pending = false;
            }
            ti += 1;
        } else if v.facts.is_compact_boundary() {
            boundary_pending = true;
        }
    }
    if recs.is_empty() {
        return None;
    }
    // Real token cost contributed by each recorded turn: within a segment it's the usage delta from
    // the previous record; at a segment start it's the whole post-compaction context (summary + that
    // turn) — real content that a resumed tail must re-send.
    let mut costs: Vec<(usize, u64)> = Vec::with_capacity(recs.len());
    let mut prev = 0u64;
    for (t, u, seg_start) in &recs {
        let cost = if *seg_start { *u } else { u.saturating_sub(prev) };
        costs.push((*t, cost));
        prev = *u;
    }
    // Accumulate real cost from the newest turn backward, stopping BEFORE the budget would be
    // exceeded (the ≤-budget contract) — this spans compaction boundaries correctly (each segment's
    // content counted once, via its deltas). If even the newest turn alone busts the budget, keep it
    // and flag the overshoot.
    let mut acc = 0u64;
    let mut start = recs.last().unwrap().0 + 1;
    let mut overshoot = false;
    let mut fits_whole = true;
    for (t, c) in costs.iter().rev() {
        if acc.saturating_add(*c) > budget {
            if acc == 0 {
                acc = *c;
                start = *t;
                overshoot = true;
            }
            fits_whole = false;
            break;
        }
        acc += c;
        start = *t;
    }
    if fits_whole {
        start = 0; // the whole session fits — keep everything, incl. turns before the first record
    }
    Some((start, acc, overshoot))
}

/// Resolve which conversational turns (`[start, end)`) a `--window`/`--range` prune keeps, plus the
/// real token size of the kept tail when known (from `usage`) and whether the ≤-budget contract had
/// to be overshot (a single turn bigger than the budget). `--range` is explicit; `--window` sizes by
/// Claude's recorded usage (falling back to a byte estimate only when usage is absent), snapped back
/// to a user turn so the tail opens on a prompt. Neither set ⇒ the whole session.
fn select_kept_turns(
    parsed: &[Option<Decision>],
    opts: &PruneOptions,
    total_turns: usize,
) -> (usize, usize, Option<u64>, bool) {
    if let Some((s, e)) = opts.keep_range {
        let end = e.unwrap_or(total_turns).min(total_turns);
        return (s.min(end), end, None, false);
    }
    let Some(budget) = opts.window else {
        return (0, total_turns, None, false);
    };

    let is_user: Vec<bool> = parsed.iter().flatten().filter_map(Decision::turn_kind).collect();
    let snap = |mut start: usize| {
        // Snap back to a user turn so the tail opens on a prompt, not a bare assistant reply.
        while start > 0 && !is_user.get(start).copied().unwrap_or(true) {
            start -= 1;
        }
        start
    };

    // PRINCIPLED path: size by Claude's own recorded token counts.
    if let Some((start, real, overshoot)) = usage_window_cutoff(parsed, budget) {
        return (snap(start), total_turns, Some(real), overshoot);
    }

    // Fallback (no usage records): byte estimate of message.content, newest-backward, keeping the
    // largest tail whose estimate is ≤ budget (same contract as the usage path).
    // Model-visible attachments (system reminders) ride with the turn they precede.
    let mut per_turn: Vec<u64> = Vec::new();
    let mut pending = 0u64;
    for v in parsed.iter().flatten().filter(|v| !v.sidechain()) {
        if v.is_main_turn() {
            per_turn.push(v.est.total() + pending);
            pending = 0;
        } else {
            pending += v.est.total();
        }
    }
    // Trailing rendered reminders still belong to the retained tail even without a following
    // provider turn. Account for them before choosing a window.
    if let Some(last) = per_turn.last_mut() {
        *last = last.saturating_add(pending);
    }
    let mut acc = 0u64;
    let mut start = per_turn.len();
    let mut overshoot = false;
    for i in (0..per_turn.len()).rev() {
        if acc.saturating_add(per_turn[i]) > budget {
            if acc == 0 {
                // Even the single newest turn exceeds the budget: keep it, but flag the overshoot.
                start = i;
                overshoot = true;
            }
            break;
        }
        acc += per_turn[i];
        start = i;
    }
    if start == per_turn.len() {
        start = 0; // no turns at all — degenerate, keep everything
    }
    let start = snap(start);
    // Reopening on the preceding user prompt can enlarge a byte-sized selection; report that
    // unavoidable coherent-tail overshoot rather than silently promising the original budget.
    let retained = per_turn[start..].iter().copied().fold(0u64, u64::saturating_add);
    (start, total_turns, None, overshoot || retained > budget)
}

/// Prune `src_path`, a `harness` transcript whose adapter declares derived copies
/// ([`crate::harness::Adapter::identity`]: Claude's `<id>.jsonl`), into a new session. Returns
/// what happened.
pub fn prune_session(harness: crate::ir::Harness, src_path: &Path, opts: &PruneOptions) -> Result<PruneResult> {
    prune_session_after_stage(harness, src_path, opts, || Ok(()))
}

fn prune_session_after_stage(
    harness: crate::ir::Harness,
    src_path: &Path,
    opts: &PruneOptions,
    after_stage: impl FnOnce() -> Result<()>,
) -> Result<PruneResult> {
    if let Some(id) = &opts.new_id {
        crate::publish::validate_output_id(id)?;
    }
    let reference = crate::ir::SessionRef::from_path(src_path, harness);
    let subagent = crate::rewind::is_subagent(&reference);
    let identity = crate::derive::identity(reference.harness)?;
    let source = crate::json::JsonSource::capture(src_path)?;
    let file = std::fs::File::open(src_path)?;
    source.check_metadata(file.metadata()?)?;
    let original_size = file.metadata()?.len();
    let mut lines = Vec::new();
    let mut parsed = Vec::new();
    let mut tool_names = HashMap::new();
    let mut plan_check = PlanCheck::default();
    // Retain only decision facts; payload trees live for one record. Preserve the old global
    // trim_end contract (including whitespace-only trailing records) without copying the file.
    let mut line_no = 0usize;
    for record in ranges::records(
        &file,
        Range {
            offset: 0,
            len: original_size,
        },
    )? {
        let (range, _) = record?;
        line_no += 1;
        // An empty line is blank like any JSON-whitespace row (a JSON range cannot be empty).
        let read = (range.len != 0).then(|| {
            source.read(ReadInput::Range(range.offset, range.len), Validation::Value, |view| {
                decide(view, range, opts, &mut tool_names, &mut plan_check)
            })
        });
        let facts = match read.transpose() {
            Ok(facts) => facts,
            Err(e) if e.is::<crate::json::InvalidJson>() => {
                // Value-compatible validation rejected this row. Invalid UTF-8 is fatal first,
                // under its own name; then only a blank row (JSON whitespace) passes, copied
                // verbatim. Anything else is a line the new session could not read back, so it
                // is refused before anything is staged, whether or not a window would drop it.
                if !ranges::valid_utf8(&mut ranges::reader(&file, range)?)? {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "stream did not contain valid UTF-8",
                    )
                    .into());
                }
                if ranges::trim(&file, range)?.len != 0 {
                    return Err(e.context(format!(
                        "{}: line {line_no} is not a complete JSON record; repair it, or cut before it \
                         with `cv rewind --at <MSG_IDX>`",
                        src_path.display()
                    )));
                }
                None
            }
            Err(e) => return Err(e),
        };
        let facts = facts.map(|mut decision| {
            // A sub-agent transcript pruned on its own is its own main thread.
            decision.facts.sidechain &= !subagent;
            decision
        });
        parsed.push(facts);
        lines.push(range);
    }
    while let Some(range) = lines.last_mut() {
        let trimmed = ranges::trim(&file, *range)?;
        if trimmed.len == 0 {
            lines.pop();
            parsed.pop();
        } else {
            // trim() locates both boundaries; only the final boundary belongs to trim_end().
            range.len = trimmed.offset + trimmed.len - range.offset;
            break;
        }
    }
    // An empty or blank-only source has nothing to keep: it once published a one-newline session
    // (an empty file) or failed on an empty JSON range (blank lines).
    if lines.is_empty() {
        bail!("{} holds no records; nothing to prune", src_path.display());
    }
    source.check()?;
    // A plan that does not fit the session is refused before anything is staged.
    let plan_warnings = match &opts.plan {
        Some(plan) => plan_check.finish(plan)?,
        None => Vec::new(),
    };

    let source_id = parsed
        .iter()
        .flatten()
        .find_map(|v| v.facts.session_id.clone())
        .unwrap_or_default();
    let parent_session_id = subagent.then(|| source_id.clone()).filter(|id| !id.is_empty());
    let source_id = if subagent { reference.id.clone() } else { source_id };
    let new_id = opts.new_id.clone().unwrap_or_else(new_uuid);
    if new_id == source_id {
        bail!("new id must differ from the source id");
    }
    if parent_session_id.as_ref().is_some_and(|parent| parent == &new_id) {
        bail!("new id must differ from the parent session id");
    }

    let project_dir = crate::rewind::project_dir(src_path);
    let dir = if subagent {
        project_dir.as_path()
    } else {
        src_path.parent().unwrap_or(Path::new("."))
    };
    let resource_dir = src_path.parent().unwrap_or(Path::new("."));
    let src_stem = src_path.file_stem().and_then(|s| s.to_str()).unwrap_or(&source_id);
    let new_path = dir.join(format!("{new_id}.jsonl"));
    let sidecar_path = dir.join(format!("{new_id}.flat.jsonl"));
    let source_location = std::fs::canonicalize(src_path)?;
    if std::fs::canonicalize(&new_path).is_ok_and(|p| p == source_location) {
        bail!("new session path must differ from the source path");
    }
    if !opts.dry_run && !opts.force {
        // Another session (or `<id>.flat`, a session's sidecar) already holds the name.
        let hint = " (pick another --to, or pass --force to replace it)";
        crate::publish::refuse_existing(&new_path, hint)?;
        crate::publish::refuse_existing(&sidecar_path, hint)?;
        if opts.copy_resources && resource_dir.join(src_stem).is_dir() {
            crate::publish::refuse_existing(&dir.join(&new_id), hint)?;
        }
    }

    // Map tool_use_id -> (name, input) from assistant tool_use blocks (which use `id`, not
    // `tool_use_id`); tool_result blocks only carry the id, not the name.

    // Conversational-turn index per line, so keep-last can spare the most recent turns. A line is
    // "old" (eligible to snip) iff its turn index is before the keep-last window. Sidechain lines
    // are positioned by the surrounding main-thread turn but never counted.
    let total_turns = parsed.iter().flatten().filter(|v| v.is_main_turn()).count();
    let keep_from = total_turns.saturating_sub(opts.keep_last);

    // `--window`/`--range`: select which conversational turns survive; the rest are dropped and the
    // first survivor is re-rooted after the loop. Defaults to the whole session (no-op).
    let (keep_start, keep_end, window_real, overshoot) = select_kept_turns(&parsed, opts, total_turns);
    let windowing = keep_start > 0 || keep_end < total_turns;
    // Session-level singleton records the kept tail must not lose (see [`SINGLETON_RECORDS`]): the
    // line index of the LAST occurrence of each type, kept wherever the window falls.
    let mut keep_singletons: HashSet<usize> = HashSet::new();
    if windowing {
        let mut last: HashMap<&str, usize> = HashMap::new();
        for (i, v) in parsed.iter().enumerate() {
            if let Some(t) = v.as_ref().map(|v| v.facts.ty.as_str()) {
                if SINGLETON_RECORDS.contains(&t) {
                    last.insert(t, i);
                }
            }
        }
        keep_singletons.extend(last.into_values());
    }
    let mut dropped_turns = 0usize;
    let mut warnings: Vec<String> = plan_warnings;
    if overshoot {
        warnings.push(format!(
            "--window {}: the newest coherent tail EXCEEDS the requested budget — kept its required \
             prompt, turn and reminders; resume may still hit the context wall",
            opts.window.unwrap_or(0),
        ));
    }

    // Revive weighs each record's estimate before and after pruning against recorded usage. Records
    // map one-to-one onto output records unless turns are dropped.
    let before: Option<Vec<Option<Est>>> =
        (opts.revive && !windowing).then(|| parsed.iter().map(|f| f.as_ref().map(|f| f.est)).collect());
    let mut output = std::io::BufWriter::with_capacity(64 * 1024, ByteStage::new());
    let mut output_facts = Vec::new();
    let mut output_pruned = Vec::new();
    let mut sidecar_stage = std::io::BufWriter::with_capacity(64 * 1024, ByteStage::new());
    let mut sidecar_count = 0usize;
    let mut rooted = false;
    let mut pruned_count = 0usize;
    let mut image_blocks = 0usize;
    let mut est_tokens_saved = 0u64;
    let mut turn = 0usize;
    let mut plan_report = opts.plan.as_ref().map(|_| PlanReport::default());
    // `--plan` attachment drops: removed after staging with their children relinked.
    let mut dropped_records: HashSet<String> = HashSet::new();

    for (idx, (facts, range)) in parsed.into_iter().zip(lines.iter()).enumerate() {
        let Some(facts) = facts else {
            // Blank records (the parse loop refused every other unreadable row) use the same
            // writer with no accepted rewrite nodes.
            let mut scan = MarkerWriter::new(&mut output);
            let selected = crate::derive::Select {
                range: *range,
                newline: true,
                record: None,
                blank: true,
                ids: Vec::new(),
                replacements: Vec::new(),
                root: false,
            };
            crate::derive::derive(
                &source,
                &file,
                std::iter::once(Ok(selected)),
                &crate::derive::Rewrite {
                    session_id: &new_id,
                    standalone: subagent,
                    identity,
                },
                &mut scan,
            )?;
            if opts.revive {
                output_facts.push(None);
                output_pruned.push(scan.found);
            }
            continue;
        };
        let mut facts = facts;
        let (user, assistant) = (facts.facts.ty == "user", facts.facts.ty == "assistant");
        let is_turn_line = user || assistant;
        let is_turn = facts.is_main_turn();
        let this_turn = turn;
        if is_turn {
            turn += 1;
        }
        let is_old = is_turn_line && this_turn < keep_from;

        // `--window`/`--range`: drop turns outside the kept range. Non-turn records (system lines,
        // attachments, file-history snapshots, sidechain lines, synthetic notices) travel with the
        // turn they precede — EXCEPT:
        //  * the last occurrence of each session-level singleton (titles, tag, agent name, cost,
        //    … — [`SINGLETON_RECORDS`]) is always kept so the new session keeps its label and
        //    identity (a `summary`'s `leafUuid` may dangle after a head drop; Claude Code tolerates
        //    that);
        //  * records trailing the FINAL kept turn are kept when the range runs to the end of the
        //    session (they belong to the kept tail, not to any dropped turn).
        if windowing {
            let drop_line = if is_turn {
                this_turn < keep_start || this_turn >= keep_end
            } else if keep_singletons.contains(&idx) {
                false
            } else {
                this_turn < keep_start || (keep_end < total_turns && this_turn >= keep_end)
            };
            if drop_line {
                if is_turn {
                    dropped_turns += 1;
                }
                continue;
            }
        }

        let needs_root = (subagent || (windowing && keep_start > 0)) && !rooted && facts.is_main_turn();
        rooted |= needs_root;
        if let (Some(report), Some(kind)) = (plan_report.as_mut(), facts.drop_attachment.take()) {
            // Stashed whole by uuid and staged as is; the removal pass takes it out and relinks.
            let uuid = facts.facts.uuid.clone().unwrap_or_default();
            if !opts.drop {
                let record = SourcePayload {
                    original: source.validated_range(range.offset, range.len)?,
                    entry: SidecarEntry {
                        id: uuid.clone(),
                        slot: "record".into(),
                        name: kind,
                        input: Value::Null,
                        content: Value::Null,
                        size: usize::try_from(range.len)?,
                        line_count: 1,
                        kind: "text".into(),
                    },
                };
                write_source_payload(&record, &mut sidecar_stage)?;
                sidecar_count += 1;
            }
            dropped_records.insert(uuid);
            report.attachments_dropped += 1;
            report.bytes_saved += range.len + 1;
            facts.est = Est::default();
        }
        // Under `--plan`, the plan alone picks results: every user line's, whatever its age.
        let results = match &opts.plan {
            Some(_) => user && facts.planned,
            None => is_old && user && facts.payload,
        };
        if let Some(report) = plan_report.as_mut() {
            report.keep += facts.plan_keep;
        }
        let may_prune =
            is_turn_line && (opts.declassify || results || (is_old && assistant && opts.thinking && facts.thinking));
        let plan = if may_prune {
            source_turn_prune(
                &source,
                *range,
                opts,
                &new_id,
                idx,
                &tool_names,
                if opts.plan.is_some() { results } else { is_old && user },
                is_old && assistant && opts.thinking,
            )?
        } else {
            None
        };
        let mut selection = crate::derive::Select {
            range: *range,
            newline: true,
            record: Some(source.validated_range(range.offset, range.len)?),
            blank: false,
            ids: std::mem::take(&mut facts.facts.ids),
            replacements: Vec::new(),
            root: needs_root,
        };
        if let Some(plan) = plan {
            selection.record = Some(plan.record);
            selection.replacements = plan.replacements;
            pruned_count += plan.payloads.len();
            est_tokens_saved += plan.saved;
            image_blocks += plan.images;
            if let Some(report) = plan_report.as_mut() {
                report.precis += plan.planned.precis;
                report.excerpt += plan.planned.excerpt;
                report.bytes_saved += plan.planned.bytes_saved;
            }
            if !opts.drop {
                for payload in &plan.payloads {
                    write_source_payload(payload, &mut sidecar_stage)?;
                    sidecar_count += 1;
                }
            }
            if opts.revive {
                facts.est = plan.estimate;
            }
        }
        let mut scan = MarkerWriter::new(&mut output);
        crate::derive::derive(
            &source,
            &file,
            std::iter::once(Ok(selection)),
            &crate::derive::Rewrite {
                session_id: &new_id,
                standalone: subagent,
                identity,
            },
            &mut scan,
        )?;
        if opts.revive {
            // A dropped attachment carries no marker, but its recorded span no longer holds it.
            let found = scan.found || facts.facts.uuid.as_ref().is_some_and(|u| dropped_records.contains(u));
            output_facts.push(Some(facts));
            output_pruned.push(found);
        }
    }

    source.check_metadata(file.metadata()?)?;
    source.check()?;
    let output = output.into_inner().map_err(|e| e.into_error())?.finish()?;
    let (output, usage_rewritten, revive_tokens, revive_old_tokens) = if opts.revive {
        revive_staged(
            &source,
            output,
            &output_facts,
            &output_pruned,
            before.as_deref(),
            windowing,
            window_real,
        )?
    } else {
        (output, 0, None, None)
    };
    // `--drop-thinking` empties thinking-only records and `--plan` drops attachments; they go,
    // their children relinked.
    let output = if opts.thinking || !dropped_records.is_empty() {
        crate::harness::claude::replay::drop_records(output, opts.thinking, &dropped_records)?
            .0
            .finish()?
    } else {
        output
    };
    let (output, skills) = skills::apply(output, opts.skills_policy, skills::config_dir_of(src_path).as_deref())?;
    let new_size = output.byte_len()?;
    let sidecar = sidecar_stage.into_inner().map_err(|e| e.into_error())?.finish()?;

    let sidecar_out = (!opts.drop && sidecar_count > 0).then(|| sidecar_path.clone());
    let copied = (opts.copy_resources && resource_dir.join(src_stem).is_dir()).then(|| dir.join(&new_id));
    if sidecar_out
        .as_ref()
        .is_some_and(|p| std::fs::canonicalize(p).is_ok_and(|p| p == source_location))
    {
        bail!("sidecar path must differ from the source path");
    }

    if !opts.dry_run {
        use crate::publish::Claim;
        let claim = if opts.force { Claim::Replace } else { Claim::New };
        let mode = crate::publish::mode_of(&file);
        let transcript = stage(&new_path, &output, mode)?;
        let sidecar_file = sidecar_out.as_ref().map(|p| stage(p, &sidecar, mode)).transpose()?;
        let resources = copied
            .as_ref()
            .map(|dst| stage_resources(&resource_dir.join(src_stem), dst))
            .transpose()?;
        after_stage()?;
        source.check_metadata(file.metadata()?)?;
        source.check()?;
        // A sub-agent's copy is a standalone session.
        let agent = crate::derive::agent_of(src_path);
        crate::derive::verify(harness, &source, &file, agent, transcript.path(), &new_path, None)?;
        // Serialization and writes complete before publication. Sidecar and transcript publish
        // as one set, the sidecar first, so a transcript never names markers its sidecar cannot
        // answer.
        let mut set = crate::publish::Set::new();
        if let Some((p, staged)) = sidecar_out.as_ref().zip(sidecar_file) {
            set.add(staged, p, claim);
        }
        set.add(transcript, &new_path, claim);
        set.publish()?;
        if opts.force && sidecar_out.is_none() {
            // The replaced session's own sidecar would answer `cv cat <new-id>` with its payloads.
            match std::fs::remove_file(&sidecar_path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("removing the replaced {}", sidecar_path.display()))
                }
                _ => {}
            }
        }
        if let Some((dst, staged)) = copied.as_ref().zip(resources) {
            // Published whole: an existing directory is replaced (with --force), never merged into.
            staged.publish(dst, claim)?;
        }
    }

    Ok(PruneResult {
        source_id,
        parent_session_id,
        new_id,
        new_path,
        sidecar_path: sidecar_out,
        copied_resources: copied,
        pruned_count,
        image_blocks,
        original_size,
        new_size,
        est_context_tokens_saved: est_tokens_saved,
        usage_rewritten,
        dropped_turns,
        window_real_tokens: window_real,
        revive_tokens,
        revive_old_tokens,
        warnings,
        skills,
        plan: plan_report,
        dry_run: opts.dry_run,
    })
}

/// Stage `bytes` beside `destination` for [`crate::publish`]; a failed write leaves nothing behind.
fn stage(destination: &Path, bytes: &ByteSource, mode: u32) -> Result<crate::publish::Staged> {
    let mut staged = crate::publish::Staged::beside(destination, "prune", mode)?;
    std::io::copy(
        &mut std::io::BufReader::with_capacity(64 * 1024, bytes.reader()?),
        &mut staged,
    )
    .with_context(|| format!("writing {}", destination.display()))?;
    Ok(staged)
}

/// One record's [`Decision`]: the codec's facts, the payload/thinking flags against `opts`, the
/// estimate when `--window`/`--revive` will weigh it, and each assistant `tool_use` block's
/// name and position into `tools` (results carry only the id). Under `--plan`, the record's
/// results and attachment kind are checked against the plan in `check`.
fn decide(
    view: crate::json::JsonView<'_>,
    range: Range,
    opts: &PruneOptions,
    tools: &mut HashMap<String, (String, Range, usize)>,
    check: &mut PlanCheck,
) -> Result<Decision> {
    use crate::harness::store::document::Fields;
    let (facts, body) = Facts::parse(view)?;
    let large = |v: crate::json::JsonView<'_>| value_byte_size_source(v).map(|n| n > opts.min_size);
    let mut payload = body.tool_use_result.map(large).transpose()?.unwrap_or(false);
    for result in &body.results {
        payload |= large(*result)?;
    }
    let thinking = facts.blocks.iter().any(|b| b.ty.as_deref() == Some("thinking"));
    let (mut planned, mut plan_keep, mut drop_attachment) = (false, 0, None);
    if let Some(plan) = &opts.plan {
        if facts.ty == "user" {
            for block in body
                .content
                .map(|c| c.array())
                .transpose()?
                .flatten()
                .into_iter()
                .flatten()
            {
                let Some(block) = Fields::parse(block?, &["type", "tool_use_id", "content"])? else {
                    continue;
                };
                if block.string("type")?.as_deref() != Some("tool_result") {
                    continue;
                }
                if let Some(id) = block.string("tool_use_id")? {
                    let (replace, keep) = check.result(plan, &id, block.get("content"))?;
                    planned |= replace;
                    plan_keep += usize::from(keep);
                }
            }
        } else if facts.ty == "attachment" && !plan.attachments.drop_kinds.is_empty() {
            let kind = match view.get("attachment")? {
                Some(attachment) => Fields::parse(attachment, &["type"])?
                    .map(|a| a.string("type"))
                    .transpose()?
                    .flatten(),
                None => None,
            };
            if let Some(kind) = kind.filter(|k| plan.attachments.drop_kinds.contains(k)) {
                if facts.uuid.is_some() {
                    drop_attachment = Some(kind);
                } else {
                    check
                        .warnings
                        .push(format!("a {kind} attachment has no uuid to stash it by; kept"));
                }
            }
        }
    }
    if facts.ty == "assistant" {
        for (index, block) in facts.blocks.iter().enumerate() {
            if let (Some("tool_use"), Some(id)) = (block.ty.as_deref(), &block.id) {
                tools.insert(id.clone(), (block.name.clone().unwrap_or_default(), range, index));
            }
        }
    }
    let mut est = Est::default();
    if opts.window.is_some() || opts.revive {
        match facts.ty.as_str() {
            "user" | "assistant" => {
                if let Some(content) = body.content {
                    est = est_content_source(content, &[])?;
                }
            }
            "attachment" => {
                if let Some(items) = body.rendered.map(|v| v.array()).transpose()?.flatten() {
                    for item in items {
                        if let Some(item) = Fields::parse(item?, &["content"])? {
                            if let Some(text) = item.get("content") {
                                if text.is_string()? {
                                    est.tokens += BLOCK_TOKENS + source_string_tokens(text)?;
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(Decision {
        facts,
        est,
        payload,
        thinking,
        planned,
        plan_keep,
        drop_attachment,
    })
}

/// A string view priced at `kind`, read in chunks (prune markers at their own rate).
fn source_str_tokens(view: crate::json::JsonView<'_>, kind: TokenKind) -> Result<u64> {
    let mut tokens = crate::ir::StrTokens::new(kind);
    view.for_each_str_chunk(|part| {
        tokens.push(part.as_bytes());
        Ok(())
    })?;
    Ok(tokens.tokens())
}
fn source_string_tokens(view: crate::json::JsonView<'_>) -> Result<u64> {
    source_str_tokens(view, TokenKind::Text)
}
fn value_byte_size_source(view: crate::json::JsonView<'_>) -> Result<usize> {
    if view.is_null()? {
        return Ok(0);
    }
    if view.is_string()? {
        let mut bytes = 0;
        view.for_each_str_chunk(|part| {
            bytes += part.len();
            Ok(())
        })?;
        Ok(bytes)
    } else {
        Ok(usize::try_from(view.canonical_len()?)?)
    }
}
/// [`est_content`] over a source view, field for field the same arithmetic, with any node in
/// `replacements` priced as its replacement.
fn est_content_source(view: crate::json::JsonView<'_>, replacements: &[(crate::json::Json, Value)]) -> Result<Est> {
    if let Some(value) = view.replacement(replacements)? {
        return Ok(est_content(value));
    }
    if view.is_string()? {
        return Ok(Est {
            tokens: BLOCK_TOKENS + source_string_tokens(view)?,
            ..Est::default()
        });
    }
    let Some(blocks) = view.array()? else {
        return Ok(Est {
            tokens: source_canonical_tokens(view, replacements, TokenKind::ToolResult)?,
            ..Est::default()
        });
    };
    let mut total = Est::default();
    for block in blocks {
        total.add(est_block_source(block?, replacements)?);
    }
    Ok(total)
}
fn source_canonical_tokens(
    view: crate::json::JsonView<'_>,
    replacements: &[(crate::json::Json, Value)],
    kind: TokenKind,
) -> Result<u64> {
    let mut tokens = crate::ir::StrTokens::new(kind);
    view.write_canonical_replacing(replacements, &mut tokens)?;
    Ok(tokens.tokens())
}
/// [`est_block`] over a source view.
fn est_block_source(block: crate::json::JsonView<'_>, replacements: &[(crate::json::Json, Value)]) -> Result<Est> {
    if let Some(value) = block.replacement(replacements)? {
        return Ok(est_block(value));
    }
    let fields = crate::harness::store::document::Fields::parse(
        block,
        &["type", "text", "thinking", "signature", "data", "input", "content"],
    )?;
    let Some(fields) = fields else {
        return Ok(Est {
            tokens: BLOCK_TOKENS + source_canonical_tokens(block, replacements, TokenKind::ToolResult)?,
            ..Est::default()
        });
    };
    let string = |key: &str, kind: TokenKind| -> Result<u64> {
        match fields.get(key) {
            Some(value) if value.is_string()? => source_str_tokens(value, kind),
            _ => Ok(0),
        }
    };
    let mut est = Est {
        tokens: BLOCK_TOKENS,
        ..Est::default()
    };
    match fields.string("type")?.as_deref() {
        Some("image") => est.images += 1,
        Some("text") => est.tokens += string("text", TokenKind::Text)?,
        Some("thinking") => {
            est.tokens += string("thinking", TokenKind::Text)?;
            est.signature += string("signature", TokenKind::Signature)?;
        }
        Some("redacted_thinking") => est.signature += string("data", TokenKind::Signature)?,
        Some("tool_use") => {
            if let Some(input) = fields.get("input") {
                let len = usize::try_from(input.canonical_len()?)?;
                est.tokens += crate::ir::est_tokens_bytes(len, TokenKind::ToolInput);
            }
        }
        Some("tool_result") => {
            let Some(content) = fields.get("content") else {
                return Ok(est);
            };
            if let Some(value) = content.replacement(replacements)? {
                est.add(est_block(&serde_json::json!({"type": "tool_result", "content": value})));
                est.tokens -= BLOCK_TOKENS;
            } else if content.is_string()? {
                est.tokens += source_str_tokens(content, TokenKind::ToolResult)?;
            } else if let Some(items) = content.array()? {
                for item in items {
                    let item = item?;
                    let item_fields = crate::harness::store::document::Fields::parse(item, &["type", "text"])?;
                    match item_fields
                        .as_ref()
                        .map(|f| f.string("type"))
                        .transpose()?
                        .flatten()
                        .as_deref()
                    {
                        Some("image") => est.images += 1,
                        Some("text") => {
                            if let Some(text) = item_fields.as_ref().and_then(|f| f.get("text")) {
                                if text.is_string()? {
                                    est.tokens += source_str_tokens(text, TokenKind::ToolResult)?;
                                }
                            }
                        }
                        _ => est.tokens += source_canonical_tokens(item, replacements, TokenKind::ToolResult)?,
                    }
                }
            } else if !content.is_null()? {
                est.tokens += source_canonical_tokens(content, replacements, TokenKind::ToolResult)?;
            }
        }
        _ => est.tokens += source_canonical_tokens(block, replacements, TokenKind::ToolResult)?,
    }
    Ok(est)
}

/// Sidecar metadata retains a source descriptor rather than its original payload bytes.
struct SourcePayload {
    entry: SidecarEntry,
    original: crate::json::Json,
}
struct SourcePrune {
    record: crate::json::Json,
    replacements: Vec<(crate::json::Json, Value)>,
    payloads: Vec<SourcePayload>,
    saved: u64,
    estimate: Est,
    images: usize,
    /// `--plan`: the précis and excerpts among `payloads`, and the payload bytes they removed.
    planned: PlanReport,
}

/// Compatibility test entry for the source user pass; turn composition uses the same planner.
#[cfg(test)]
fn source_user_prune(
    source: &crate::json::JsonSource,
    range: Range,
    opts: &PruneOptions,
    new_id: &str,
    idx: usize,
    tools: &HashMap<String, (String, Range, usize)>,
) -> Result<Option<SourcePrune>> {
    source_turn_prune(source, range, opts, new_id, idx, tools, true, false)
}

#[allow(clippy::too_many_arguments)]
fn source_turn_prune(
    source: &crate::json::JsonSource,
    range: Range,
    opts: &PruneOptions,
    new_id: &str,
    idx: usize,
    tools: &HashMap<String, (String, Range, usize)>,
    user_old: bool,
    thinking_old: bool,
) -> Result<Option<SourcePrune>> {
    use crate::harness::store::document::Fields;
    let record = source.validated_range(range.offset, range.len)?;
    let planned = record.with_view(|view| {
        let Some(top) = Fields::parse(view, &["message", "uuid", "toolUseResult", "sessionId", "session_id"])? else {
            return Ok(None);
        };
        let message = top
            .get("message")
            .map(|value| Fields::parse(value, &["content"]))
            .transpose()?
            .flatten();
        let content = message.as_ref().and_then(|message| message.get("content"));
        let blocks = content.map(|value| value.array()).transpose()?.flatten();
        let key = top.string("uuid")?.unwrap_or_else(|| format!("L{idx}"));
        let mut replacements = Vec::new();
        let mut payloads = Vec::new();
        let mut declassified = Vec::new();
        let mut saved = 0;
        let mut block_sources = Vec::new();
        let mut projected_blocks = HashMap::new();
        let mut projected_content = None;
        let mut images = 0;
        let mut mirror_base = None;
        let mut mirror_tuid = None;
        let mut omitted = HashSet::new();
        let mut kept_blocks = Vec::new();
        let mut planned_report = PlanReport::default();
        let mut planned_any = false;
        for (index, original_block) in blocks.into_iter().flatten().enumerate() {
            let original_block = original_block?;
            if opts.revive {
                block_sources.push(original_block.to_json()?);
            }
            let Some(block) = Fields::parse(original_block, &["type", "tool_use_id", "content", "thinking", "text"])?
            else {
                continue;
            };
            let ty = block.string("type")?;
            let mut projected: Option<Value> = None;
            if thinking_old && ty.as_deref() == Some("thinking") {
                // Omitted, as on the owned path; the array is rewritten once the loop is done.
                let size = value_byte_size_source(original_block)?;
                let lines = block
                    .get("thinking")
                    .map(source_text_stats)
                    .transpose()?
                    .map(|stats| stats.lines())
                    .unwrap_or(1);
                saved += est_block_source(original_block, &[])?.total();
                payloads.push(source_payload(
                    original_block.to_json()?,
                    format!("{key}#think{index}"),
                    "thinking",
                    "thinking",
                    size,
                    lines,
                ));
                omitted.insert(index);
                continue;
            }
            kept_blocks.push(original_block.to_json()?);
            if opts.declassify && ty.as_deref() == Some("text") {
                let text = block.get("text");
                let (stats, hits) = source_declassify_stats(text, opts)?;
                if hits >= opts.declassify_min_hits {
                    let id = format!("{key}#declass{index}");
                    let marker = build_marker(
                        &id,
                        "declassified",
                        &Value::Null,
                        "text",
                        stats.bytes,
                        stats.lines(),
                        new_id,
                        opts.drop,
                    );
                    saved +=
                        crate::ir::est_tokens_bytes(stats.bytes, TokenKind::Text).saturating_sub(str_tokens(&marker));
                    declassified.push(source_payload(
                        original_block.to_json()?,
                        id,
                        "content",
                        "declassified",
                        stats.bytes,
                        stats.lines(),
                    ));
                    projected = Some(serde_json::json!({"type":"text", "text":marker}));
                }
            }
            if let Some(projected) = projected {
                if opts.revive {
                    projected_blocks.insert(index, projected.clone());
                }
                replacements.push((original_block.to_json()?, projected));
                continue;
            }
            if !user_old || ty.as_deref() != Some("tool_result") {
                continue;
            }
            let tuid = block.string("tool_use_id")?;
            let id = tuid.clone().unwrap_or_else(|| format!("{key}#tr{index}"));
            if mirror_base.is_none() {
                mirror_base = Some(id.clone());
                mirror_tuid = tuid.clone();
            }
            // Under `--plan` only a planned précis or excerpt is replaced, whatever its size.
            let planned = match &opts.plan {
                Some(plan) => match plan.get(tuid.as_deref()) {
                    Some(p @ (plan::Planned::Precis { .. } | plan::Planned::Excerpt { .. }))
                        if matches!(plan::effect(&id, block.get("content"))?, plan::Effect::Replace) =>
                    {
                        Some(p)
                    }
                    _ => continue,
                },
                None => None,
            };
            let Some(content) = block.get("content") else { continue };
            if content.is_null()? {
                continue;
            }
            let stats = if content.is_string()? {
                Some(source_text_stats(content)?)
            } else {
                None
            };
            let size = match &stats {
                Some(stats) => stats.bytes,
                None => value_byte_size_source(content)?,
            };
            if planned.is_none()
                && (size <= opts.min_size
                    || stats
                        .as_ref()
                        .is_some_and(|stats| stats.prefix.starts_with(MARKER_PREFIX)))
            {
                continue;
            }
            let (kind, line_count, img, original_tokens) = match &stats {
                Some(stats) => (
                    "text".into(),
                    stats.lines(),
                    0,
                    crate::ir::est_tokens_bytes(stats.bytes, TokenKind::ToolResult),
                ),
                None => classify_source(content)?,
            };
            if kind == "none" {
                continue;
            }
            let (name, input) = source_tool_metadata(source, tuid.as_deref(), tools)?;
            let marker = build_marker(&id, &name, &input, &kind, size, line_count, new_id, opts.drop);
            let marker = match planned {
                Some(planned) => {
                    let text = match planned {
                        plan::Planned::Excerpt { .. } => match plan::payload_text(content)? {
                            Ok(text) => Some(text),
                            Err(kind) => bail!("{id}: an excerpt needs a text payload; this one is {kind}"),
                        },
                        _ => None,
                    };
                    let replaced = plan::replacement(&marker, planned, text.as_deref());
                    match planned {
                        plan::Planned::Excerpt { .. } => planned_report.excerpt += 1,
                        _ => planned_report.precis += 1,
                    }
                    planned_report.bytes_saved += (size as u64).saturating_sub(replaced.len() as u64);
                    planned_any = true;
                    replaced
                }
                None => marker,
            };
            saved += original_tokens.saturating_sub(str_tokens(&marker));
            images += img;
            let original = content.to_json()?;
            replacements.push((original.clone(), Value::String(marker)));
            payloads.push(SourcePayload {
                original,
                entry: SidecarEntry {
                    id,
                    slot: "content".into(),
                    name,
                    input,
                    content: Value::Null,
                    size,
                    line_count,
                    kind,
                },
            });
        }
        if opts.declassify && content.map(|value| value.is_string()).transpose()?.unwrap_or(false) {
            let (stats, hits) = source_declassify_stats(content, opts)?;
            if hits >= opts.declassify_min_hits {
                let id = format!("{key}#declass");
                let marker = build_marker(
                    &id,
                    "declassified",
                    &Value::Null,
                    "text",
                    stats.bytes,
                    stats.lines(),
                    new_id,
                    opts.drop,
                );
                saved += crate::ir::est_tokens_bytes(stats.bytes, TokenKind::Text).saturating_sub(str_tokens(&marker));
                let original = content.unwrap().to_json()?;
                declassified.push(source_payload(
                    original.clone(),
                    id,
                    "content",
                    "declassified",
                    stats.bytes,
                    stats.lines(),
                ));
                if opts.revive {
                    projected_content = Some(Value::String(marker.clone()));
                }
                replacements.push((original, Value::String(marker)));
            }
        }
        if let Some(mirror) = top.get("toolUseResult").filter(|_| user_old) {
            if !mirror.is_null()? {
                let stats = if mirror.is_string()? {
                    Some(source_text_stats(mirror)?)
                } else {
                    None
                };
                let size = match &stats {
                    Some(stats) => stats.bytes,
                    None => value_byte_size_source(mirror)?,
                };
                // A planned result's mirror goes with it, whatever its size: the record must not
                // carry the full text twice. Unplanned mirrors stay under `--plan`.
                let eligible = match &opts.plan {
                    Some(_) => planned_any,
                    None => size > opts.min_size,
                };
                if eligible
                    && !stats
                        .as_ref()
                        .is_some_and(|stats| stats.prefix.starts_with(MARKER_PREFIX))
                {
                    let id = format!("{}{TUR_SUFFIX}", mirror_base.unwrap_or_else(|| key.clone()));
                    let kind = if tur_is_image_source(mirror)? { "image" } else { "text" };
                    let line_count = stats.as_ref().map(SourceTextStats::lines).unwrap_or(1);
                    let (name, input) = source_tool_metadata(source, mirror_tuid.as_deref(), tools)?;
                    let marker = build_marker(&id, &name, &input, kind, size, line_count, new_id, opts.drop);
                    let original = mirror.to_json()?;
                    if planned_any {
                        planned_report.bytes_saved += (size as u64).saturating_sub(marker.len() as u64);
                    }
                    replacements.push((original.clone(), Value::String(marker)));
                    payloads.push(SourcePayload {
                        original,
                        entry: SidecarEntry {
                            id,
                            slot: "toolUseResult".into(),
                            name,
                            input,
                            content: Value::Null,
                            size,
                            line_count,
                            kind: kind.into(),
                        },
                    });
                }
            }
        }
        payloads.extend(declassified);
        if payloads.is_empty() {
            return Ok(None);
        }
        if !omitted.is_empty() {
            // Claude Code writes one block per record, so this is usually `[]` (the record is then
            // removed after staging). A rarer mixed record materializes its kept blocks once.
            let mut array = Vec::with_capacity(kept_blocks.len());
            for block in &kept_blocks {
                let mut bytes = Vec::new();
                block.with_view(|view| view.write_canonical_replacing(&replacements, &mut bytes))?;
                array.push(serde_json::from_slice(&bytes)?);
            }
            if let Some(content) = content {
                replacements.push((content.to_json()?, Value::Array(array)));
            }
        }
        let mut estimate = if opts.revive && !content.map(|value| value.is_array()).transpose()?.unwrap_or(false) {
            match projected_content.as_ref() {
                Some(value) => est_content(value),
                None => content
                    .map(|c| est_content_source(c, &[]))
                    .transpose()?
                    .unwrap_or_default(),
            }
        } else {
            Est::default()
        };
        if opts.revive {
            for (index, block) in block_sources.into_iter().enumerate() {
                if omitted.contains(&index) {
                    continue;
                }
                if let Some(projected) = projected_blocks.get(&index) {
                    estimate.add(est_block(projected));
                    continue;
                }
                block.with_view(|view| {
                    estimate.add(est_block_source(view, &replacements)?);
                    Ok(())
                })?;
            }
        }
        Ok(Some(SourcePrune {
            record: record.clone(),
            replacements,
            payloads,
            saved,
            estimate,
            images,
            planned: planned_report,
        }))
    })?;
    Ok(planned)
}
fn source_payload(
    original: crate::Json,
    id: String,
    slot: &str,
    name: &str,
    size: usize,
    line_count: usize,
) -> SourcePayload {
    SourcePayload {
        original,
        entry: SidecarEntry {
            id,
            slot: slot.into(),
            name: name.into(),
            input: Value::Null,
            content: Value::Null,
            size,
            line_count,
            kind: "text".into(),
        },
    }
}

#[derive(Default)]
struct SourceTextStats {
    bytes: usize,
    newlines: usize,
    last: Option<u8>,
    prefix: String,
}
impl SourceTextStats {
    fn append(&mut self, text: &str) {
        if self.prefix.len() < MARKER_PREFIX.len() {
            self.prefix
                .extend(text.chars().take(MARKER_PREFIX.len() - self.prefix.len()));
        }
        self.bytes += text.len();
        self.newlines += memchr::memchr_iter(b'\n', text.as_bytes()).count();
        if let Some(byte) = text.as_bytes().last() {
            self.last = Some(*byte);
        }
    }
    fn lines(&self) -> usize {
        (self.newlines + usize::from(self.last.is_some_and(|byte| byte != b'\n'))).max(1)
    }
}
fn source_text_stats(value: crate::json::JsonView<'_>) -> Result<SourceTextStats> {
    let mut stats = SourceTextStats::default();
    value.for_each_str_chunk(|chunk| {
        stats.append(chunk);
        Ok(())
    })?;
    Ok(stats)
}
fn classify_source(value: crate::json::JsonView<'_>) -> Result<(String, usize, usize, u64)> {
    use crate::harness::store::document::Fields;
    if value.is_string()? {
        let stats = source_text_stats(value)?;
        return Ok((
            "text".into(),
            stats.lines(),
            0,
            crate::ir::est_tokens_bytes(stats.bytes, TokenKind::ToolResult),
        ));
    }
    let Some(blocks) = value.array()? else {
        return Ok(("none".into(), 1, 0, 0));
    };
    let mut text = SourceTextStats::default();
    let mut images = 0;
    let mut tokens = 0;
    let mut any = false;
    for block in blocks {
        any = true;
        let original = block?;
        let Some(block) = Fields::parse(original, &["type", "text"])? else {
            tokens += crate::ir::est_tokens_bytes(usize::try_from(original.canonical_len()?)?, TokenKind::ToolResult);
            continue;
        };
        match block.string("type")?.as_deref() {
            Some("text") => {
                if let Some(value) = block.get("text") {
                    if value.is_string()? {
                        if text.bytes != 0 {
                            text.append("\n");
                        }
                        let before = text.bytes;
                        value.for_each_str_chunk(|chunk| {
                            text.append(chunk);
                            Ok(())
                        })?;
                        tokens += crate::ir::est_tokens_bytes(text.bytes - before, TokenKind::ToolResult);
                    }
                }
            }
            Some("image") => {
                images += 1;
                tokens += IMAGE_TOKENS;
            }
            _ => {
                tokens +=
                    crate::ir::est_tokens_bytes(usize::try_from(original.canonical_len()?)?, TokenKind::ToolResult);
            }
        }
    }
    let kind = match (images > 0, text.bytes > 0, any) {
        (true, true, _) => "mixed",
        (true, false, _) => "image",
        (false, true, _) | (false, false, true) => "text",
        _ => "none",
    };
    Ok((kind.into(), text.lines(), images, tokens))
}
fn source_tool_metadata(
    source: &crate::json::JsonSource,
    id: Option<&str>,
    tools: &HashMap<String, (String, Range, usize)>,
) -> Result<(String, Value)> {
    let Some((name, range, index)) = id.and_then(|id| tools.get(id)) else {
        return Ok(("unknown".into(), Value::Null));
    };
    let input = source.validated_range(range.offset, range.len)?.with_view(|tool| {
        let Some(message) = tool.get("message")? else {
            return Ok(Value::Null);
        };
        let Some(content) = message.get("content")? else {
            return Ok(Value::Null);
        };
        let Some(mut blocks) = content.array()? else {
            return Ok(Value::Null);
        };
        let Some(block) = blocks.nth(*index).transpose()? else {
            return Ok(Value::Null);
        };
        Ok(block
            .get("input")?
            .map(|input| input.resolve().map(|value| value.into_owned()))
            .transpose()?
            .unwrap_or(Value::Null))
    })?;
    Ok((name.clone(), input))
}
fn tur_is_image_source(value: crate::json::JsonView<'_>) -> Result<bool> {
    use crate::harness::store::document::Fields;
    let Some(fields) = Fields::parse(value, &["type", "file"])? else {
        return Ok(false);
    };
    if fields.string("type")?.as_deref() == Some("image") {
        return Ok(true);
    }
    let Some(file) = fields
        .get("file")
        .map(|file| Fields::parse(file, &["base64", "type"]))
        .transpose()?
        .flatten()
    else {
        return Ok(false);
    };
    Ok(file.get("base64").is_some() || file.string("type")?.is_some_and(|ty| ty.starts_with("image/")))
}

/// One sidecar line, written field by field in [`SidecarEntry`]'s order, the original payload
/// through the shared canonical source writer.
fn write_source_payload(payload: &SourcePayload, writer: &mut impl Write) -> Result<()> {
    let e = &payload.entry;
    let mut o = crate::harness::wire::write::ObjectWriter::new(writer)?;
    o.field("id", &e.id)?;
    o.field("slot", &e.slot)?;
    o.field("name", &e.name)?;
    o.field("input", &e.input)?;
    o.field_with("content", |mut w| payload.original.write_canonical(&mut w))?;
    o.field("size", e.size)?;
    o.field("line_count", e.line_count)?;
    o.field("kind", &e.kind)?;
    o.finish()?;
    writer.write_all(b"\n")?;
    Ok(())
}

/// Marker evidence follows emitted bytes, including source chunks split by id surgery.
struct MarkerWriter<'a, W> {
    output: &'a mut W,
    tail: Vec<u8>,
    found: bool,
}
impl<'a, W: Write> MarkerWriter<'a, W> {
    fn new(output: &'a mut W) -> Self {
        Self {
            output,
            tail: Vec::new(),
            found: false,
        }
    }
}
impl<W: Write> Write for MarkerWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = self.output.write(bytes)?;
        let bytes = &bytes[..n];
        if !self.found {
            let marker = MARKER_PREFIX.as_bytes();
            let mut boundary = self.tail.clone();
            boundary.extend_from_slice(&bytes[..bytes.len().min(marker.len() - 1)]);
            self.found =
                boundary.windows(marker.len()).any(|p| p == marker) || bytes.windows(marker.len()).any(|p| p == marker);
            // Keep only a marker's worth of stream bytes: the head of a chunk never joins the
            // tail wholesale, so the copy stays bounded by `keep`, not by the stream.
            let keep = marker.len() - 1;
            if self.tail.len() + bytes.len() <= keep {
                self.tail.extend_from_slice(bytes);
            } else if bytes.len() >= keep {
                self.tail.clear();
                self.tail.extend_from_slice(&bytes[bytes.len() - keep..]);
            } else {
                self.tail.drain(..self.tail.len() + bytes.len() - keep);
                self.tail.extend_from_slice(bytes);
            }
        }
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

/// Rewrite trailing recorded usage using an estimated post-prune prompt size (`--revive`).
/// Claude Code checks saved usage before making another provider request. The estimate combines
/// recorded-usage deltas across transcript spans with byte estimates for the content after the last
/// compaction boundary, preferring available recorded evidence. Trailing records that exceed this
/// estimate are rewritten (including nested iteration usage). This changes saved gate metadata;
/// it neither measures the new request nor verifies provider acceptance or behavior on resume.
/// Returns `(records rewritten, estimated tokens written, prior maximum recorded tokens)`.
fn revive_staged(
    source: &crate::json::JsonSource,
    output: ByteSource,
    parsed: &[Option<Decision>],
    pruned: &[bool],
    before: Option<&[Option<Est>]>,
    windowed: bool,
    real_override: Option<u64>,
) -> Result<(ByteSource, usize, Option<u64>, Option<u64>)> {
    let window = if windowed {
        0
    } else {
        parsed
            .iter()
            .rposition(|p| p.as_ref().is_some_and(|v| v.facts.is_compact_boundary()))
            .map_or(0, |i| i + 1)
    };
    let honest = before
        .filter(|before| before.len() == parsed.len())
        .and_then(|before| anchored_window_tokens(parsed, before, window))
        .unwrap_or_else(|| honest_window_tokens(parsed, pruned, window, window == 0 && !windowed))
        .max(real_override.unwrap_or(0));
    let mut rewritten = 0;
    let mut stale_max = 0;
    let mut stage = std::io::BufWriter::with_capacity(64 * 1024, ByteStage::new());
    use std::io::BufRead;
    let output = std::sync::Arc::new(output);
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, output.reader()?);
    let mut position = 0u64;
    for (i, facts) in parsed.iter().enumerate() {
        let total = facts
            .as_ref()
            .filter(|v| !v.sidechain())
            .and_then(|v| v.facts.usage_in)
            .unwrap_or(0);
        let rewrite = i >= window && total > honest;
        let mut start = None;
        let mut end = position;
        // Frame one private staged row without constructing its bytes. Rewritten rows need a
        // trimmed complete-value range; untouched rows still copy their exact framing directly.
        loop {
            let bytes = reader.fill_buf()?;
            anyhow::ensure!(!bytes.is_empty(), "staged transcript ended before its decision roster");
            let newline = memchr::memchr(b'\n', bytes);
            let n = newline.map_or(bytes.len(), |i| i + 1);
            if rewrite {
                let non_whitespace = |byte: &u8| !crate::harness::store::json::is_whitespace(byte);
                if start.is_none() {
                    start = bytes[..n]
                        .iter()
                        .position(non_whitespace)
                        .map(|offset| position + offset as u64);
                }
                if let Some(offset) = bytes[..n].iter().rposition(non_whitespace) {
                    end = position + offset as u64 + 1;
                }
            } else {
                stage.write_all(&bytes[..n])?;
            }
            reader.consume(n);
            position += n as u64;
            if newline.is_some() {
                break;
            }
        }
        if rewrite {
            let start = start.context("usage row has no JSON value")?;
            source.read(
                ReadInput::Backing(output.clone(), start, end - start),
                Validation::Value,
                |view| write_source_usage(view, total, honest, &mut stage),
            )?;
            stage.write_all(b"\n")?;
            rewritten += 1;
            stale_max = stale_max.max(total);
        }
    }
    Ok((
        stage.into_inner().map_err(|e| e.into_error())?.finish()?,
        rewritten,
        (rewritten > 0).then_some(honest),
        (rewritten > 0).then_some(stale_max),
    ))
}

/// Keep source bodies untouched while applying the established owned usage correction. Only
/// demanded usage metadata is materialized; rich/oversized usage itself retains that fallback.
fn write_source_usage(view: crate::json::JsonView<'_>, total: u64, honest: u64, writer: &mut impl Write) -> Result<()> {
    let usage = view
        .get("message")?
        .map(|message| message.get("usage"))
        .transpose()?
        .flatten();
    let mut replacements = Vec::new();
    if let Some(usage) = usage {
        let mut owned = usage.resolve()?.into_owned();
        if let Some(object) = owned.as_object_mut() {
            pin_usage(object, honest);
            replacements.push((usage.to_json()?, owned));
        }
    }
    // Value's insertion order and final duplicate acceptance remain unchanged. An absent
    // _cv_orig_ctx appends after existing keys, just as Value indexing does. Source children
    // use the shared canonical writer, including the projected usage replacement.
    anyhow::ensure!(view.is_object()?, "usage row is not an object");
    let insert = view.get("_cv_orig_ctx")?.is_none();
    let mut first = true;
    writer.write_all(b"{")?;
    view.for_each_entry(|key, child| {
        if !first {
            writer.write_all(b",")?;
        }
        serde_json::to_writer(&mut *writer, key)?;
        writer.write_all(b":")?;
        child.write_canonical_replacing(&replacements, writer)?;
        first = false;
        Ok(())
    })?;
    if insert {
        if !first {
            writer.write_all(b",")?;
        }
        writer.write_all(b"\"_cv_orig_ctx\":")?;
        serde_json::to_writer(&mut *writer, &total)?;
    }
    writer.write_all(b"}")?;
    Ok(())
}

/// Share of an estimated removal revive credits (measured removals ran up to 1.35× over on the
/// claude-opus-4-6 tokenizer before [`crate::ir::tokenizer_scale`], 1.1× after).
const REVIVE_REMOVAL_CREDIT: f64 = 0.8;
/// Added content (markers, the turns after the last recorded prompt) is charged this much extra.
const REVIVE_ADDED_MARGIN: f64 = 0.1;
/// Final margin on the estimate: a pruned session near the window is held back rather than
/// refused (calibration in the commit that set it).
const REVIVE_SAFETY_MARGIN: f64 = 0.05;
/// Models whose resumed requests were measured counting old thinking signatures (claude-opus-4-6,
/// -4-7, -4-8, -opus-5, -fable-5, all resumed by Claude Code 2.1.299). Removing them frees context
/// only there; for others (claude-fable-5-1, -opus-5-5, -sonnet-5 measured, and any unknown model)
/// revive credits nothing, though their recorded usage once counted them.
const SIGNATURES_COUNTED: [&str; 5] = [
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-fable-5",
];
/// Even there the transcript holds more signatures than Claude Code sends (it leaves some out),
/// so a removed signature is credited at this share on top of [`REVIVE_REMOVAL_CREDIT`].
const SIGNATURE_CREDIT: f64 = 0.6;

/// The fixed prompt (system prompt, tool definitions) when no trustworthy recorded prompt implies
/// it: Claude Code 2.1.299 sent 19–36k tokens of it with no MCP servers configured.
const UNKNOWN_OVERHEAD_TOKENS: u64 = 20_000;

/// The post-prune prompt size, anchored on recorded usage: the last recorded prompt, adjusted by
/// what pruning changed before it, plus everything after it; floored by an outright estimate of
/// the resumed branch plus the fixed overhead the recorded prompts imply.
///
/// A recorded prompt is not evidence when it is the first after the window's compaction boundary:
/// it still carries the pre-compaction size revive exists to correct. With no other, only the
/// outright estimate stands.
///
/// Removed thinking signatures are credited only for [`SIGNATURES_COUNTED`] models; everywhere
/// else they are still counted as present (the estimate errs high). `None` when the window holds
/// no turns.
fn anchored_window_tokens(parsed: &[Option<Decision>], before: &[Option<Est>], window: usize) -> Option<u64> {
    // Claude Code resumes the branch ending at the newest turn; records on abandoned branches
    // (edits, rewinds) are not sent, so their pruning frees nothing.
    let chain = resumed_branch(parsed, window);
    let main = |i: usize| {
        parsed[i]
            .as_ref()
            .filter(|v| !v.sidechain() && chain.as_ref().is_none_or(|c| c.contains(&i)))
    };
    let lines = |lo: usize, hi: usize| (lo..hi).filter(move |i| main(*i).is_some());
    let pre = |i: usize| before[i].unwrap_or_default();
    let post = |i: usize| main(i).map(|v| v.est).unwrap_or_default();
    lines(window, parsed.len()).next()?;
    // One entry per provider response: the first record carrying its prompt size.
    let mut responses: Vec<(usize, u64)> = Vec::new();
    for i in lines(window, parsed.len()) {
        let total = main(i)?.recorded();
        if total > 0 && responses.last().is_none_or(|(_, t)| *t != total) {
            responses.push((i, total));
        }
    }
    if window > 0 && !responses.is_empty() {
        responses.remove(0);
    }
    let model = (window..parsed.len())
        .rev()
        .filter_map(main)
        .find_map(|v| v.facts.model.as_deref().filter(|m| *m != "<synthetic>"))
        .unwrap_or("");
    let scale = crate::ir::tokenizer_scale(model);
    let size = Est::total;
    let signature_credit = if SIGNATURES_COUNTED.contains(&model) {
        SIGNATURE_CREDIT
    } else {
        0.0
    };

    // Floor: the whole resumed branch, estimated outright, plus the fixed overhead the recorded
    // prompts imply — the smallest excess of any over the content before it, since a transcript
    // that began from history held elsewhere implies far more.
    let mut overhead = None::<u64>;
    let mut prefix = Est::default();
    let mut next = window;
    for &(at, total) in &responses {
        for i in lines(next, at) {
            prefix.add(pre(i));
        }
        next = at;
        let implied = total.saturating_sub(size(prefix));
        overhead = Some(overhead.map_or(implied, |o| o.min(implied)));
    }
    let mut whole = Est::default();
    for i in lines(window, parsed.len()) {
        whole.add(post(i));
    }
    let overhead = overhead.unwrap_or(UNKNOWN_OVERHEAD_TOKENS);
    let mut estimate = (overhead + size(whole)) as f64 / scale;

    if let Some(&(anchor, recorded)) = responses.last() {
        let mut anchored = recorded as f64;
        for i in lines(window, anchor) {
            let (a, b) = (pre(i), post(i));
            let images = (b.images as f64 - a.images as f64) * IMAGE_TOKENS as f64;
            let signature = b.signature as f64 - a.signature as f64;
            for (delta, credit) in [
                (b.tokens as f64 - a.tokens as f64, 1.0),
                (signature, signature_credit),
                (images, 0.5),
            ] {
                anchored += if delta < 0.0 {
                    delta * credit * REVIVE_REMOVAL_CREDIT / scale
                } else {
                    delta * (1.0 + REVIVE_ADDED_MARGIN)
                };
            }
        }
        for i in lines(anchor, parsed.len()) {
            anchored += size(post(i)) as f64 * (1.0 + REVIVE_ADDED_MARGIN);
        }
        estimate = estimate.max(anchored);
    }
    Some((estimate * (1.0 + REVIVE_SAFETY_MARGIN)).ceil() as u64)
}

/// Record indices on the branch ending at the window's newest turn, walking `parentUuid` (or
/// `logicalParentUuid`) back. `None` when the newest turn records no `parentUuid` (no tree to walk).
fn resumed_branch(parsed: &[Option<Decision>], window: usize) -> Option<HashSet<usize>> {
    let facts = |i: usize| parsed[i].as_ref().map(|v| &v.facts);
    let mut by_id = HashMap::new();
    for i in window..parsed.len() {
        if let Some(uuid) = facts(i).and_then(|f| f.uuid.as_deref()) {
            by_id.insert(uuid, i);
        }
    }
    let leaf = (window..parsed.len()).rev().find(|&i| {
        parsed[i].as_ref().is_some_and(|v| {
            !v.sidechain() && matches!(v.facts.ty.as_str(), "user" | "assistant") && v.facts.uuid.is_some()
        })
    })?;
    facts(leaf)?.link()?;
    let mut chain = HashSet::new();
    let mut at = Some(leaf);
    while let Some(i) = at.filter(|i| chain.insert(*i)) {
        at = facts(i)
            .and_then(|f| f.link().flatten())
            .and_then(|p| by_id.get(p).copied());
    }
    Some(chain)
}

/// Honest post-prune token size of `parsed[window..]`, combining recorded-usage deltas with byte
/// estimates, **span by span** (a span = the lines covered by one usage record, i.e. everything
/// since the previous record).
///
///  * A span whose content was NOT touched by pruning: the recorded delta is real evidence of its
///    cost, usually HIGHER than the byte estimate — take `max(delta, estimate)` so we never
///    write a figure lower than what the recorded evidence supports.
///  * A span holding a `[PRUNED …]` marker: the recorded delta measured the now-snipped payload, so
///    only the byte estimate of what's actually left is meaningful.
///  * A segment-start record (right after a compaction boundary, or the scan start when
///    `!first_trusted`): its absolute total is not a delta — and after a boundary it's exactly the
///    stale figure revive exists to correct — so its span is byte-estimated too.
///  * Lines after the last record (or when there are no records at all): byte estimate.
///
/// Sidechain lines contribute neither usage nor bytes — they aren't re-sent on the main thread.
fn honest_window_tokens(parsed: &[Option<Decision>], pruned: &[bool], window: usize, first_trusted: bool) -> u64 {
    let span_est = |lo: usize, hi: usize| -> u64 {
        parsed[lo..hi]
            .iter()
            .flatten()
            .filter(|v| !v.sidechain())
            .map(|v| v.est.total())
            .sum()
    };
    let mut sum = 0u64;
    let mut prev = 0u64;
    let mut seg_start = !first_trusted;
    let mut span_start = window;
    for i in window..parsed.len() {
        let Some(v) = &parsed[i] else { continue };
        if v.facts.is_compact_boundary() {
            seg_start = true;
            continue;
        }
        if v.sidechain() {
            continue;
        }
        let total = v.recorded();
        if total == 0 {
            continue;
        }
        let est = span_est(span_start, i + 1);
        let span_pruned = pruned[span_start..=i].iter().any(|p| *p);
        sum += if seg_start || span_pruned {
            est
        } else {
            est.max(total.saturating_sub(prev))
        };
        prev = total;
        seg_start = false;
        span_start = i + 1;
    }
    sum + span_est(span_start, parsed.len())
}

/// `advisor_message` / `compaction` iterations are side requests Claude Code skips when reading
/// context size.
fn is_aux_iteration(it: &Value) -> bool {
    matches!(
        it.get("type").and_then(Value::as_str),
        Some("advisor_message" | "compaction")
    )
}

/// Pin a `usage` record to `honest` context tokens everywhere Claude Code might read it: the
/// top-level counters AND every non-auxiliary entry of `iterations`. Pinning only the top level
/// used to be enough; since Claude Code 2.1.277 prefers the last request iteration, a stale
/// iteration silently defeats the pin — the gate reads the old wall figure and refuses the session
/// client-side (a synthesized "Prompt is too long", no API call). Cache counters are zeroed (a fresh
/// resume has no live cache anyway); output counts are real and left alone.
fn pin_usage(u: &mut serde_json::Map<String, Value>, honest: u64) {
    fn pin(m: &mut serde_json::Map<String, Value>, honest: u64) {
        m.insert("input_tokens".into(), Value::from(honest));
        m.insert("cache_read_input_tokens".into(), Value::from(0u64));
        m.insert("cache_creation_input_tokens".into(), Value::from(0u64));
        if let Some(cc) = m.get_mut("cache_creation").and_then(Value::as_object_mut) {
            cc.insert("ephemeral_1h_input_tokens".into(), Value::from(0u64));
            cc.insert("ephemeral_5m_input_tokens".into(), Value::from(0u64));
        }
    }
    pin(u, honest);
    if let Some(iters) = u.get_mut("iterations").and_then(Value::as_array_mut) {
        for it in iters.iter_mut().filter(|it| !is_aux_iteration(it)) {
            if let Some(m) = it.as_object_mut() {
                pin(m, honest);
            }
        }
    }
}

mod declassify;
use declassify::source_declassify_stats;
mod plan;
use plan::PlanCheck;
pub use plan::{Plan, PlanAttachments, PlanRefused, PlanReport, Planned};
/// Snip the eligible payloads on one (old) user line, in place. `line_key` (the line's uuid or a
/// file-position fallback) keys sidecar ids for blocks that carry no `tool_use_id` of their own.
mod retrieve;
pub use retrieve::retrieve;
#[cfg(test)]
use retrieve::retrieve_captured;

// ── helpers ───────────────────────────────────────────────────────────────

/// A new session id: v7 (time-ordered), as cv mints every other id.
pub(crate) fn new_uuid() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn str_tokens(s: &str) -> u64 {
    crate::ir::est_str_tokens(s, TokenKind::Text)
}

/// One record's estimated prompt contribution, in the parts revive weighs differently: whether
/// signatures count is decided per session from recorded usage, and removed images are credited
/// cautiously (one image measured anywhere from 1.4k to 4.2k tokens).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Est {
    /// Everything but signatures and images.
    tokens: u64,
    signature: u64,
    images: u64,
}
impl Est {
    fn add(&mut self, other: Est) {
        self.tokens += other.tokens;
        self.signature += other.signature;
        self.images += other.images;
    }
    fn total(self) -> u64 {
        self.tokens + self.signature + self.images * IMAGE_TOKENS
    }
}

/// The same estimator handles owned replacement nodes through an owned view.
fn est_content(content: &Value) -> Est {
    est_content_source(content.into(), &[]).expect("owned JSON has no fallible backing")
}
fn est_block(block: &Value) -> Est {
    est_block_source(block.into(), &[]).expect("owned JSON has no fallible backing")
}

/// A compact key=value summary of the tool's most telling arguments, for the marker.
fn summarize_args(input: &Value) -> String {
    let Some(obj) = input.as_object() else {
        return String::new();
    };
    let key_args = [
        "file_path",
        "command",
        "pattern",
        "query",
        "url",
        "path",
        "description",
        "prompt",
    ];
    let mut picked: Vec<String> = obj
        .iter()
        .filter(|(k, _)| key_args.contains(&k.as_str()))
        .take(2)
        .map(|(k, v)| fmt_arg(k, v))
        .collect();
    if picked.is_empty() {
        if let Some((k, v)) = obj.iter().next() {
            picked.push(fmt_arg(k, v));
        }
    }
    picked.join(", ")
}

fn fmt_arg(k: &str, v: &Value) -> String {
    let s = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
    // Truncate on a char boundary (values can contain multibyte UTF-8).
    let s = if s.chars().count() > 80 {
        format!("{}...", s.chars().take(77).collect::<String>())
    } else {
        s
    };
    format!("{k}={s}")
}

#[allow(clippy::too_many_arguments)]
fn build_marker(
    id: &str,
    name: &str,
    input: &Value,
    kind: &str,
    size: usize,
    lines: usize,
    session: &str,
    dropped: bool,
) -> String {
    let args = summarize_args(input);
    let args = if args.is_empty() {
        String::new()
    } else {
        format!(" {args}")
    };
    let label = match kind {
        "image" => "image",
        "mixed" => "text+image",
        _ => "text",
    };
    // Under --drop the payload is destroyed — never advertise a retrieval that can't work.
    let tail = if dropped {
        "dropped (no sidecar)".to_string()
    } else {
        format!("retrieve: cv cat {session} {id}")
    };
    format!("{MARKER_PREFIX}{id} tool={name}{args} | {label} {size}B/{lines}L | session={session} | {tail}]")
}

/// Recursively copy a directory tree (used to carry `subagents/`/`workflows/` into the new session).
/// Copy the session's resource directory into a private staged directory beside `dst`.
fn stage_resources(src: &Path, dst: &Path) -> Result<crate::publish::StagedDir> {
    // Replacing `dst` deletes the tree it held; that must never hold the source.
    if let (Ok(src), Ok(dst)) = (std::fs::canonicalize(src), std::fs::canonicalize(dst)) {
        anyhow::ensure!(
            !src.starts_with(dst),
            "resource destination contains the source resources"
        );
    }
    let staged = crate::publish::StagedDir::beside(dst, "prune-resources")?;
    copy_dir(src, staged.path()).with_context(|| format!("copying session resources to {}", dst.display()))?;
    Ok(staged)
}

/// Copy `src` into `dst`, a directory this operation just created: every entry is new.
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir(&to)?;
            copy_dir(&from, &to)?;
        } else {
            let mut source = std::fs::File::open(&from)?;
            let mut target = std::fs::OpenOptions::new().write(true).create_new(true).open(&to)?;
            std::io::copy(&mut source, &mut target)?;
            target.set_permissions(source.metadata()?.permissions())?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
#[path = "prune/sidecar_tests.rs"]
mod sidecar_tests;

#[cfg(test)]
#[path = "prune/plan_tests.rs"]
mod plan_tests;
