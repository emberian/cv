//! Compaction & store doctor: attribute context-window pressure across the active post-compaction
//! window vs lifetime transcript history, and inspect local store/index/storage health.
//!
//! Powers both `cv doctor` (CLI rendering) and the `doctor` JSON surface. Sizes stored transcript
//! content with the shared estimator ([`crate::ir::est_tokens`]: per-kind bytes/token calibrated
//! against the API's token counts). Thinking counts plaintext plus signature and encrypted bytes,
//! which Claude requests do carry.
//!
//! Two complementary views are produced per report:
//! 1. **Active Post-Compaction Window (`active_window`)**: slices the active branch from the most
//!    recent [`MessageKind::CompactionBoundary`] / [`MessageKind::CompactionSummary`] to the tip,
//!    isolating surviving tool results, active thinking, current-window system reminders, token
//!    burn velocity per turn, and `system_and_tools_overhead =
//!    first_post_boundary_or_turn1_prompt_tokens.saturating_sub(reconstructed_visible_turn_tokens)`.
//! 2. **Lifetime History (`history` and top-level cumulative fields)**: totals across the entire
//!    stored transcript including pre-compaction turns and branches.

use crate::compaction::CompactionSink;
use crate::ir::{est_tokens_resolved, is_mcp_tool, Block, Message, MessageKind, Role};
use crate::stream::{Flow, MessageSink};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

/// The bucket name for an injected-context turn: the harness's own attachment kind when it recorded
/// one ([`crate::harness::Adapter::attachment_kind`], e.g. Claude's `hook_success`), else the IR
/// kind name.
fn injected_kind(m: &Message) -> String {
    crate::harness::attachment_kind(m)
        .map(str::to_string)
        .unwrap_or_else(|| m.kind.as_str().to_string())
}

#[derive(Default, Debug, Clone, Serialize)]
pub struct ToolStat {
    pub calls: u64,
    pub result_tokens: u64,
    pub is_mcp: bool,
}

/// Attribution and usage sizing for the **active post-compaction context window** (sliced along
/// the active branch from the latest `CompactionBoundary` / `CompactionSummary` to tip).
#[derive(Default, Debug, Clone, Serialize)]
pub struct ActiveWindowReport {
    /// `true` when at least one folded session's active window begins at a compaction boundary or summary.
    pub from_compaction: bool,
    /// Count of folded sessions whose active window begins after a compaction.
    pub compacted_sessions: u64,
    /// Messages in the active post-compaction window (including harness markers in that span).
    pub messages: u64,
    /// Conversational turns in the active post-compaction window.
    pub turns: u64,
    /// Seed summary tokens from [`MessageKind::CompactionSummary`] in the active window.
    pub compaction_summary: u64,
    pub user_text: u64,
    pub assistant_text: u64,
    pub thinking: u64,
    pub tool_call_args: u64,
    pub tool_results: u64,
    pub images: u64,
    pub system_text: u64,
    /// Current-window system reminders (`InjectedContext` records).
    pub attachments: u64,
    pub by_attachment: BTreeMap<String, u64>,
    pub by_tool: BTreeMap<String, ToolStat>,
    /// First nonzero recorded prompt usage in the active window (latest folded session when >1).
    pub first_prompt_tokens: Option<u64>,
    /// Reconstructed visible transcript tokens preceding that first prompt usage reading.
    pub reconstructed_visible_turn_tokens: u64,
    /// True fixed overhead estimate (`first_prompt_tokens - reconstructed_visible_turn_tokens`;
    /// median across sessions when multiple sessions are folded).
    pub system_and_tools_overhead: Option<u64>,
    /// Most recent nonzero recorded prompt usage in the active window.
    pub current_context_tokens: Option<u64>,
    /// Peak recorded prompt usage within the active window.
    pub peak_context_tokens: u64,
    /// Estimated or usage-measured token burn velocity per turn in the active window.
    pub token_burn_per_turn: u64,
    #[serde(skip)]
    session_overheads: Vec<u64>,
    #[serde(skip)]
    measured_burn_delta_sum: u64,
    #[serde(skip)]
    measured_burn_steps: u64,
}

impl ActiveWindowReport {
    /// Total estimated model-visible tokens currently surviving in the active window.
    pub fn conv_total(&self) -> u64 {
        self.compaction_summary
            + self.user_text
            + self.assistant_text
            + self.thinking
            + self.tool_call_args
            + self.tool_results
            + self.images
            + self.system_text
            + self.attachments
    }

    /// Surviving MCP tool-result tokens in the active window.
    pub fn mcp_tool_results(&self) -> u64 {
        self.by_tool
            .values()
            .filter(|s| s.is_mcp)
            .map(|s| s.result_tokens)
            .sum()
    }

    pub fn to_json(&self) -> Value {
        let tools: Vec<Value> = self
            .by_tool
            .iter()
            .map(|(name, s)| {
                json!({
                    "tool": name,
                    "mcp": s.is_mcp,
                    "calls": s.calls,
                    "result_tokens": s.result_tokens,
                })
            })
            .collect();
        let mcp = self.mcp_tool_results();
        json!({
            "from_compaction": self.from_compaction,
            "compacted_sessions": self.compacted_sessions,
            "messages": self.messages,
            "turns": self.turns,
            "current_context": self.current_context_tokens,
            "peak_context": self.peak_context_tokens,
            "first_prompt_tokens": self.first_prompt_tokens,
            "reconstructed_visible_turn_tokens": self.reconstructed_visible_turn_tokens,
            "system_and_tools_overhead": self.system_and_tools_overhead,
            "token_burn_per_turn": self.token_burn_per_turn,
            "conversational": {
                "total": self.conv_total(),
                "compaction_summary": self.compaction_summary,
                "tool_results": self.tool_results,
                "thinking": self.thinking,
                "assistant_text": self.assistant_text,
                "user_text": self.user_text,
                "tool_call_args": self.tool_call_args,
                "images": self.images,
                "system_msgs": self.system_text,
                "system_reminders": self.attachments,
            },
            "by_attachment": self.by_attachment,
            "tool_results_mcp": mcp,
            "tool_results_builtin": self.tool_results.saturating_sub(mcp),
            "by_tool": tools,
        })
    }
}

/// A context-attribution report. Accumulate sessions with [`Report::session`], then render
/// (CLI) or serialize ([`Report::to_json`], for `--json` and the MCP tool).
#[derive(Default, Debug, Clone, Serialize)]
pub struct Report {
    pub sessions: u64,
    pub messages: u64,
    // Estimated token equivalents of cumulative stored content, by source (lifetime history)
    pub user_text: u64,
    pub assistant_text: u64,
    /// Stored thinking text plus opaque signature/encrypted payload byte estimates.
    pub thinking: u64,
    pub tool_call_args: u64,
    pub tool_results: u64,
    pub images: u64,
    pub system_text: u64,
    /// System reminders Claude Code appended to the prompt (model-visible `attachment` records).
    pub attachments: u64,
    /// The same, by attachment kind (`hook_success`, `edited_text_file`, `queued_command`, …).
    pub by_attachment: BTreeMap<String, u64>,
    pub by_tool: BTreeMap<String, ToolStat>,
    // compaction
    pub compactions: u64,
    pub auto_compactions: u64,
    pub pre_tokens: Vec<u64>,
    // usage-derived context sizing (lifetime history)
    pub startup_ctx: u64, // maximum first nonzero recorded prompt usage; includes conversation
    pub peak_ctx: u64,    // maximum recorded prompt usage across turns
    /// Active post-compaction window slice (active branch from latest compaction to tip).
    pub active_window: ActiveWindowReport,
}

impl Report {
    /// Start folding one session into the report. Stream its messages into the returned sink (a
    /// [`Demand::FACTS`](crate::Demand::FACTS) pass: attribution counts
    /// decoded text in bounded chunks, and attachment/compaction metadata live in `extra`), then
    /// call [`SessionPass::finish`]. Adapters without a facts projection retain their full metadata.
    pub fn session(&mut self, source: Option<std::path::PathBuf>) -> SessionPass<'_> {
        self.sessions += 1;
        SessionPass {
            rep: self,
            resolver: crate::lazy::Resolver::new(source),
            names: HashMap::new(),
            unnamed: HashMap::new(),
            startup: None,
            compactions: CompactionSink::new(false),
            msg_facts: Vec::new(),
            latch: Default::default(),
        }
    }

    /// Total estimated token equivalents across stored transcript history, including compactions.
    pub fn conv_total(&self) -> u64 {
        self.user_text
            + self.assistant_text
            + self.thinking
            + self.tool_call_args
            + self.tool_results
            + self.images
            + self.system_text
            + self.attachments
    }

    /// Tool-result tokens attributable to MCP tools ([`is_mcp_tool`]).
    pub fn mcp_tool_results(&self) -> u64 {
        self.by_tool
            .values()
            .filter(|s| s.is_mcp)
            .map(|s| s.result_tokens)
            .sum()
    }

    /// The machine-readable diagnosis — `cv doctor --json` and the MCP `doctor` tool share this.
    /// Top-level cumulative fields remain intact alongside explicit `active_window` (post-compaction
    /// active branch) and `history` (lifetime transcript totals).
    pub fn to_json(&self) -> Value {
        let tools: Vec<Value> = self
            .by_tool
            .iter()
            .map(|(name, s)| json!({"tool": name, "mcp": s.is_mcp, "calls": s.calls, "result_tokens": s.result_tokens}))
            .collect();
        let mcp = self.mcp_tool_results();
        let lifetime_conv = json!({
            "total": self.conv_total(),
            "tool_results": self.tool_results,
            "thinking": self.thinking,
            "assistant_text": self.assistant_text,
            "user_text": self.user_text,
            "tool_call_args": self.tool_call_args,
            "images": self.images,
            "system_msgs": self.system_text,
            "system_reminders": self.attachments,
        });
        let history = json!({
            "messages": self.messages,
            "compactions": self.compactions,
            "auto_compactions": self.auto_compactions,
            "pre_tokens": self.pre_tokens,
            "fixed_overhead_est": self.startup_ctx,
            "peak_context": self.peak_ctx,
            "conversational": lifetime_conv.clone(),
            "by_attachment": self.by_attachment,
            "tool_results_mcp": mcp,
            "tool_results_builtin": self.tool_results.saturating_sub(mcp),
            "by_tool": tools.clone(),
        });
        json!({
            "sessions": self.sessions,
            "messages": self.messages,
            "compactions": self.compactions,
            "auto_compactions": self.auto_compactions,
            "pre_tokens": self.pre_tokens,
            "fixed_overhead_est": self.startup_ctx,
            "peak_context": self.peak_ctx,
            "conversational": lifetime_conv,
            "by_attachment": self.by_attachment,
            "tool_results_mcp": mcp,
            "tool_results_builtin": self.tool_results.saturating_sub(mcp),
            "by_tool": tools,
            "active_window": self.active_window.to_json(),
            "history": history,
        })
    }
}

/// Compact per-message attribution facts retained during a single session pass so the active
/// post-compaction branch can be sliced accurately at [`SessionPass::finish`] without retaining
/// transcript text in memory.
struct MsgFact {
    id: Option<String>,
    parent_id: Option<String>,
    role: Role,
    kind: MessageKind,
    visible: bool,
    prompt_tokens: Option<u64>,
    compaction_summary: u64,
    user_text: u64,
    assistant_text: u64,
    thinking: u64,
    tool_call_args: u64,
    tool_results: u64,
    images: u64,
    system_text: u64,
    attachments: u64,
    by_attachment: Vec<(String, u64)>,
    tool_calls: Vec<(String, bool)>,
    named_tool_results: Vec<(String, u64)>,
    unnamed_tool_results: Vec<(String, u64)>,
}

impl MsgFact {
    fn visible_tokens(&self) -> u64 {
        if !self.visible {
            return 0;
        }
        self.compaction_summary
            + self.user_text
            + self.assistant_text
            + self.thinking
            + self.tool_call_args
            + self.tool_results
            + self.images
            + self.system_text
            + self.attachments
    }
}

/// One session streaming into a [`Report`] (see [`Report::session`]): content attribution, the
/// usage-derived context sizing and the compaction tally, folded per message.
pub struct SessionPass<'a> {
    rep: &'a mut Report,
    resolver: crate::lazy::Resolver,
    /// `tool_use_id` → tool name, the last call per id wins. A result usually carries only the
    /// id, so results attribute to real tools (and MCP names), not "(unknown tool)".
    names: HashMap<String, String>,
    /// Result tokens by `tool_use_id` for results that named no tool, attributed at
    /// [`finish`](SessionPass::finish): a call can stream past after its result (a resumed
    /// transcript re-copies history), so the id is resolved against the whole session.
    unnamed: HashMap<String, u64>,
    /// First nonzero recorded prompt usage, including conversation.
    startup: Option<u64>,
    compactions: CompactionSink,
    msg_facts: Vec<MsgFact>,
    latch: crate::stream::Latch,
}

impl SessionPass<'_> {
    /// Fold the session's compactions, active-window slice, and overhead estimates into the report.
    pub fn finish(mut self) -> anyhow::Result<()> {
        if let Some(error) = self.latch.take("doctor source read") {
            return Err(error);
        }
        let rep = &mut *self.rep;
        for (id, t) in self.unnamed {
            let name = self.names.get(&id).map_or("(unknown tool)", String::as_str);
            let e = rep.by_tool.entry(name.to_string()).or_default();
            e.result_tokens += t;
            e.is_mcp |= is_mcp_tool(name);
        }
        for c in self.compactions.into_boundaries()? {
            rep.compactions += 1;
            if c.trigger.as_deref() == Some("auto") {
                rep.auto_compactions += 1;
            }
            if let Some(p) = c.pre_tokens {
                rep.pre_tokens.push(p);
            }
        }
        rep.startup_ctx = rep.startup_ctx.max(self.startup.unwrap_or(0));

        fold_active_window(&mut rep.active_window, &self.msg_facts, &self.names);
        Ok(())
    }

    fn observe(&mut self, m: &Message) -> anyhow::Result<()> {
        let rep = &mut *self.rep;
        rep.messages += 1;
        let prompt_tokens = m.usage.as_ref().map(crate::ir::Usage::prompt_tokens).filter(|&t| t > 0);
        if let Some(total) = prompt_tokens {
            rep.peak_ctx = rep.peak_ctx.max(total);
            self.startup.get_or_insert(total);
        }

        let mut fact = MsgFact {
            id: m.id.clone(),
            parent_id: m.parent_id.clone(),
            role: m.role,
            kind: m.kind,
            visible: m.is_model_visible(),
            prompt_tokens,
            compaction_summary: 0,
            user_text: 0,
            assistant_text: 0,
            thinking: 0,
            tool_call_args: 0,
            tool_results: 0,
            images: 0,
            system_text: 0,
            attachments: 0,
            by_attachment: Vec::new(),
            tool_calls: Vec::new(),
            named_tool_results: Vec::new(),
            unnamed_tool_results: Vec::new(),
        };

        // Harness-only records can carry any block type. They remain transcript/usage and
        // compaction evidence, but none of their payload contributes to model context.
        if !m.is_model_visible() {
            self.msg_facts.push(fact);
            return Ok(());
        }
        for b in &m.content {
            // What this block costs, by the one shared rule; the arms below only decide which
            // bucket the number lands in.
            let tokens = || est_tokens_resolved(b, &self.resolver);
            match b {
                Block::Text { .. } => {
                    let t = tokens()?;
                    match (m.role, m.kind) {
                        // Context the harness injected into the prompt (system reminders, hook
                        // output, turn context): real per-turn context, itemized by kind — it
                        // used to hide inside "fixed overhead".
                        (_, MessageKind::InjectedContext) => {
                            let kind = injected_kind(m);
                            rep.attachments += t;
                            *rep.by_attachment.entry(kind.clone()).or_default() += t;
                            fact.attachments += t;
                            fact.by_attachment.push((kind, t));
                        }
                        (_, MessageKind::CompactionSummary) => {
                            match m.role {
                                Role::Assistant => rep.assistant_text += t,
                                Role::System => rep.system_text += t,
                                _ => rep.user_text += t,
                            }
                            fact.compaction_summary += t;
                        }
                        (Role::Assistant, _) => {
                            rep.assistant_text += t;
                            fact.assistant_text += t;
                        }
                        (Role::System, _) => {
                            rep.system_text += t;
                            fact.system_text += t;
                        }
                        _ => {
                            rep.user_text += t;
                            fact.user_text += t;
                        }
                    }
                }
                Block::Thinking { .. } => {
                    let t = tokens()?;
                    rep.thinking += t;
                    fact.thinking += t;
                }
                Block::ToolUse { id, name, .. } => {
                    let t = tokens()?;
                    rep.tool_call_args += t;
                    fact.tool_call_args += t;
                    let mcp = is_mcp_tool(name);
                    self.names.insert(id.clone(), name.clone());
                    let e = rep.by_tool.entry(name.clone()).or_default();
                    e.calls += 1;
                    e.is_mcp |= mcp;
                    fact.tool_calls.push((name.clone(), mcp));
                }
                Block::ToolResult {
                    tool_name, tool_use_id, ..
                } => {
                    let t = tokens()?;
                    rep.tool_results += t;
                    fact.tool_results += t;
                    match tool_name {
                        Some(name) => {
                            let e = rep.by_tool.entry(name.clone()).or_default();
                            e.result_tokens += t;
                            e.is_mcp |= is_mcp_tool(name);
                            fact.named_tool_results.push((name.clone(), t));
                        }
                        None => {
                            *self.unnamed.entry(tool_use_id.clone()).or_default() += t;
                            fact.unnamed_tool_results.push((tool_use_id.clone(), t));
                        }
                    }
                }
                Block::Image { .. } => {
                    let t = tokens()?;
                    rep.images += t;
                    fact.images += t;
                }
                Block::File { .. } => {}
            }
        }
        self.msg_facts.push(fact);
        Ok(())
    }
}

/// Slice `facts` along the active branch from the latest `CompactionBoundary` / `CompactionSummary`
/// to tip and fold into `out`.
fn fold_active_window(out: &mut ActiveWindowReport, facts: &[MsgFact], names: &HashMap<String, String>) {
    if facts.is_empty() {
        return;
    }

    let branch = slice_active_branch(facts);
    if branch.is_empty() {
        return;
    }

    // Locate the most recent compaction cut on the active branch:
    // - A `CompactionBoundary` at `k` starts the post-compaction window at `k + 1` (including its
    //   paired `CompactionSummary` seed right after it).
    // - A standalone `CompactionSummary` at `s` with no preceding boundary starts the window at `s`
    //   (inclusive, so the summary seed itself is attributed to the new window).
    let last_boundary = branch.iter().rposition(|m| m.kind == MessageKind::CompactionBoundary);
    let last_summary = branch.iter().rposition(|m| m.kind == MessageKind::CompactionSummary);

    let (win_start, from_compaction) = match (last_boundary, last_summary) {
        (Some(b), Some(s)) => {
            let summary_paired_to_b =
                s == b + 1 || (s > b && branch[b].id.is_some() && branch[s].parent_id == branch[b].id);
            if s > b && !summary_paired_to_b {
                (s, true)
            } else {
                (b + 1, true)
            }
        }
        (Some(b), None) => (b + 1, true),
        (None, Some(s)) => (s, true),
        (None, None) => (0, false),
    };

    if from_compaction {
        out.from_compaction = true;
        out.compacted_sessions += 1;
    }

    let win = &branch[win_start.min(branch.len())..];
    out.messages += win.len() as u64;

    let mut user_exchange_turns = 0u64;
    let mut assistant_turns = 0u64;
    let mut prompt_readings: Vec<(usize, u64)> = Vec::new();
    let mut session_win_conv_total = 0u64;

    for (idx, m) in win.iter().enumerate() {
        if let Some(pt) = m.prompt_tokens {
            prompt_readings.push((idx, pt));
            out.peak_context_tokens = out.peak_context_tokens.max(pt);
        }
        if !m.visible {
            continue;
        }
        if m.role == Role::User && m.kind.is_exchange() {
            user_exchange_turns += 1;
        } else if m.role == Role::Assistant {
            assistant_turns += 1;
        }

        out.compaction_summary += m.compaction_summary;
        out.user_text += m.user_text;
        out.assistant_text += m.assistant_text;
        out.thinking += m.thinking;
        out.tool_call_args += m.tool_call_args;
        out.tool_results += m.tool_results;
        out.images += m.images;
        out.system_text += m.system_text;
        out.attachments += m.attachments;
        session_win_conv_total += m.visible_tokens();

        for (kind, tok) in &m.by_attachment {
            *out.by_attachment.entry(kind.clone()).or_default() += *tok;
        }
        for (name, mcp) in &m.tool_calls {
            let e = out.by_tool.entry(name.clone()).or_default();
            e.calls += 1;
            e.is_mcp |= *mcp;
        }
        for (name, tok) in &m.named_tool_results {
            let e = out.by_tool.entry(name.clone()).or_default();
            e.result_tokens += *tok;
            e.is_mcp |= is_mcp_tool(name);
        }
        for (tool_use_id, tok) in &m.unnamed_tool_results {
            let name = names.get(tool_use_id).map_or("(unknown tool)", String::as_str);
            let e = out.by_tool.entry(name.to_string()).or_default();
            e.result_tokens += *tok;
            e.is_mcp |= is_mcp_tool(name);
        }
    }

    let session_turns = if user_exchange_turns > 0 {
        user_exchange_turns
    } else if assistant_turns > 0 {
        assistant_turns
    } else if !win.is_empty() {
        1
    } else {
        0
    };
    out.turns += session_turns;

    if let Some(&(first_idx, first_pt)) = prompt_readings.first() {
        let reconstructed_before: u64 = win[..first_idx].iter().map(|m| m.visible_tokens()).sum();
        let overhead = first_pt.saturating_sub(reconstructed_before);
        out.first_prompt_tokens = Some(first_pt);
        out.reconstructed_visible_turn_tokens = reconstructed_before;
        out.session_overheads.push(overhead);
        out.system_and_tools_overhead = median_u64(&out.session_overheads);
    }

    if let Some(&(_, last_pt)) = prompt_readings.last() {
        out.current_context_tokens = Some(last_pt);
    }

    if prompt_readings.len() >= 2 {
        let first_pt = prompt_readings.first().map(|x| x.1).unwrap_or(0);
        let last_pt = prompt_readings.last().map(|x| x.1).unwrap_or(0);
        if last_pt > first_pt {
            out.measured_burn_delta_sum += last_pt - first_pt;
            out.measured_burn_steps += (prompt_readings.len() - 1) as u64;
        }
    }

    out.token_burn_per_turn = out
        .measured_burn_delta_sum
        .checked_div(out.measured_burn_steps)
        .or_else(|| out.conv_total().checked_div(out.turns))
        .unwrap_or(session_win_conv_total);
}

fn median_u64(vals: &[u64]) -> Option<u64> {
    if vals.is_empty() {
        return None;
    }
    let mut sorted = vals.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if !sorted.len().is_multiple_of(2) {
        Some(sorted[mid])
    } else {
        Some((sorted[mid - 1] + sorted[mid]) / 2)
    }
}

/// Prune abandoned sibling forks when `facts` carries an explicit `id`/`parent_id` tree with
/// multiple conversational children sharing one parent; preserve linear transcripts verbatim.
fn slice_active_branch(facts: &[MsgFact]) -> Vec<&MsgFact> {
    let mut id_to_idx: HashMap<&str, usize> = HashMap::new();
    for (idx, m) in facts.iter().enumerate() {
        if let Some(id) = m.id.as_deref().filter(|s| !s.is_empty()) {
            id_to_idx.insert(id, idx);
        }
    }
    if id_to_idx.is_empty() {
        return facts.iter().collect();
    }

    // Detect whether any parent in `id_to_idx` has >= 2 non-injected children (an actual fork).
    let mut child_counts: HashMap<usize, usize> = HashMap::new();
    let mut has_fork = false;
    for m in facts {
        if m.kind == MessageKind::InjectedContext || m.kind == MessageKind::CompactionSummary {
            continue;
        }
        if let Some(pid) = m.parent_id.as_deref() {
            if let Some(&p_idx) = id_to_idx.get(pid) {
                let c = child_counts.entry(p_idx).or_insert(0);
                *c += 1;
                if *c >= 2 {
                    has_fork = true;
                }
            }
        }
    }
    if !has_fork {
        return facts.iter().collect();
    }

    // Walk `parent_id` backwards from the last chain-participating message to mark the active spine.
    let tip_idx = facts
        .iter()
        .rposition(|m| m.parent_id.as_deref().is_some_and(|pid| id_to_idx.contains_key(pid)))
        .or_else(|| facts.iter().rposition(|m| m.id.is_some()));

    let Some(mut cur) = tip_idx else {
        return facts.iter().collect();
    };

    let mut on_spine = vec![false; facts.len()];
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(cur) {
            break;
        }
        on_spine[cur] = true;
        let Some(pid) = facts[cur].parent_id.as_deref() else {
            break;
        };
        let Some(&p_idx) = id_to_idx.get(pid) else {
            break;
        };
        cur = p_idx;
    }

    facts
        .iter()
        .enumerate()
        .filter_map(|(idx, m)| {
            if on_spine[idx] {
                return Some(m);
            }
            // Retain leaf attachments / compaction summaries / unchained harness markers attached
            // directly to an active-spine node.
            if matches!(m.kind, MessageKind::InjectedContext | MessageKind::CompactionSummary)
                && m.parent_id
                    .as_deref()
                    .and_then(|pid| id_to_idx.get(pid))
                    .is_some_and(|&p_idx| on_spine[p_idx])
            {
                return Some(m);
            }
            if m.id.is_none() && m.parent_id.is_none() {
                return Some(m);
            }
            None
        })
        .collect()
}

impl MessageSink for SessionPass<'_> {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.latch.take("doctor source read")
    }

    fn message(&mut self, m: Message) -> Flow {
        if self.latch.failed() {
            return Flow::Stop;
        }
        if let Err(error) = self.observe(&m) {
            self.latch.fail(error);
            return Flow::Stop;
        }
        self.compactions.message(m)
    }
}

// ───────────────────────────── Store & System Health ─────────────────────────────

#[cfg(feature = "sqlite")]
#[derive(Debug, Clone, Serialize)]
pub struct StateDbHealth {
    pub path: Option<String>,
    pub exists: bool,
    pub quick_check: String,
    pub bytes: u64,
    pub wal_bytes: u64,
    pub capture_inbox_backlog: u64,
    pub pack_count: u64,
    pub pack_bytes: u64,
}

#[cfg(feature = "sqlite")]
#[derive(Debug, Clone, Serialize)]
pub struct WriterVersionHealth {
    pub harness: String,
    pub verified: String,
    pub newest: Option<String>,
    pub sessions: usize,
    pub unknown: usize,
    pub newer_than_verified: bool,
}

#[cfg(feature = "sqlite")]
#[derive(Debug, Clone, Serialize)]
pub struct CatalogDbHealth {
    pub path: Option<String>,
    pub exists: bool,
    pub quick_check: String,
    pub bytes: u64,
    pub wal_bytes: u64,
    pub session_rows: u64,
    pub writer_versions: Vec<WriterVersionHealth>,
}

#[cfg(feature = "sqlite")]
#[derive(Debug, Clone, Serialize)]
pub struct IndexDirHealth {
    pub path: Option<String>,
    pub present: bool,
    pub bytes: u64,
    pub files: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clev_segments: Option<u64>,
}

#[cfg(feature = "sqlite")]
#[derive(Debug, Clone, Serialize)]
pub struct SearchIndexesHealth {
    pub fts: IndexDirHealth,
    pub trigram: IndexDirHealth,
    pub columnar_events: IndexDirHealth,
}

#[cfg(feature = "sqlite")]
#[derive(Debug, Clone, Serialize)]
pub struct StorageHealth {
    pub path: Option<String>,
    pub device_class: String,
    pub max_streams: usize,
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
}

#[cfg(feature = "sqlite")]
#[derive(Debug, Clone, Serialize)]
pub struct SkippedSourceHealth {
    pub harness: String,
    pub path: String,
    pub why: String,
}

#[cfg(feature = "sqlite")]
#[derive(Debug, Clone, Serialize)]
pub struct StoreHealthReport {
    pub sqlite_version: String,
    pub state_db: StateDbHealth,
    pub catalog_db: CatalogDbHealth,
    pub search_indexes: SearchIndexesHealth,
    pub storage: StorageHealth,
    pub skipped_sources: Vec<SkippedSourceHealth>,
}

#[cfg(feature = "sqlite")]
fn wal_sidecar(db_path: &std::path::Path) -> std::path::PathBuf {
    let mut os = db_path.as_os_str().to_os_string();
    os.push("-wal");
    std::path::PathBuf::from(os)
}

#[cfg(feature = "sqlite")]
fn file_len_or_zero(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .map_or(0, |m| m.len())
}

#[cfg(feature = "sqlite")]
fn sqlite_quick_check(conn: &rusqlite::Connection) -> String {
    match conn.query_row("PRAGMA quick_check(1)", [], |r| r.get::<_, String>(0)) {
        Ok(s) => s,
        Err(e) => format!("error: {e}"),
    }
}

#[cfg(feature = "sqlite")]
fn inspect_index_dir(dir: Option<std::path::PathBuf>, count_clev: bool) -> IndexDirHealth {
    let Some(path) = dir else {
        return IndexDirHealth {
            path: None,
            present: false,
            bytes: 0,
            files: 0,
            clev_segments: count_clev.then_some(0),
        };
    };
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut clev = 0u64;
    let mut stack = vec![path.clone()];
    while let Some(cur) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&cur) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let ep = entry.path();
            if ft.is_dir() {
                stack.push(ep);
            } else if ft.is_file() {
                files += 1;
                bytes += entry.metadata().map_or(0, |m| m.len());
                if count_clev && ep.extension().and_then(|s| s.to_str()).is_some_and(|ext| ext == "clev") {
                    clev += 1;
                }
            }
        }
    }
    IndexDirHealth {
        path: Some(path.display().to_string()),
        present: files > 0,
        bytes,
        files,
        clev_segments: count_clev.then_some(clev),
    }
}

#[cfg(feature = "sqlite")]
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
fn statvfs_space(path: &std::path::Path) -> (Option<u64>, Option<u64>) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return (None, None);
        };
        let mut vfs = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        // SAFETY: `c_path` is a valid NUL-terminated path and `vfs` is writable `statvfs` storage.
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), vfs.as_mut_ptr()) };
        if rc != 0 {
            return (None, None);
        }
        // SAFETY: `statvfs` returned 0 and initialized `vfs`.
        let vfs = unsafe { vfs.assume_init() };
        let frsize = vfs.f_frsize as u64;
        let bsize = vfs.f_bsize as u64;
        let unit = if frsize > 0 { frsize } else { bsize };
        let free = (vfs.f_bavail as u64).saturating_mul(unit);
        let total = (vfs.f_blocks as u64).saturating_mul(unit);
        (Some(free), Some(total))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        (None, None)
    }
}

/// Strictly read-only store, index, and storage health probe (`state.db`, `catalog.db`, search
/// indexes, `IoAdmission` device class, and `statvfs` free disk space). Never creates directories
/// or SQLite files on disk.
#[cfg(feature = "sqlite")]
pub fn collect_store_health() -> StoreHealthReport {
    let env = crate::env::Env::current();
    let cv_home = env.cv_home();
    let cv_cache = env.cv_cache();

    // 1. Authoritative `state.db`
    let state_db = match crate::state::StateStore::default_store() {
        Ok(store) => {
            let db_path = store.path().to_path_buf();
            let bytes = file_len_or_zero(&db_path);
            let wal_bytes = file_len_or_zero(&wal_sidecar(&db_path));
            let exists = db_path.is_file();
            let (quick_check, capture_inbox_backlog, pack_count, pack_bytes) = if !exists || bytes == 0 {
                ("absent".to_string(), 0, 0, 0)
            } else {
                match crate::harness::store::sqlite::open_ro(&db_path) {
                    Ok(reader) => match reader.read_snapshot(|conn, _reads| {
                        let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
                        if ver == 0 {
                            return Ok(("uninitialized".to_string(), 0u64, 0u64, 0u64));
                        }
                        let qc = sqlite_quick_check(conn);
                        let inbox = conn
                            .query_row("SELECT COUNT(*) FROM capture_inbox", [], |r| r.get::<_, i64>(0))
                            .map(|n| n.max(0) as u64)
                            .unwrap_or(0);
                        let (p_count, p_bytes) = conn
                            .query_row(
                                "SELECT COUNT(*), COALESCE(SUM(byte_len), 0) FROM pack_manifests",
                                [],
                                |r| Ok((r.get::<_, i64>(0)?.max(0) as u64, r.get::<_, i64>(1)?.max(0) as u64)),
                            )
                            .unwrap_or((0, 0));
                        Ok((qc, inbox, p_count, p_bytes))
                    }) {
                        Ok(tuple) => tuple,
                        Err(e) => (format!("error: {e}"), 0, 0, 0),
                    },
                    Err(e) => (format!("error: {e}"), 0, 0, 0),
                }
            };
            StateDbHealth {
                path: Some(db_path.display().to_string()),
                exists,
                quick_check,
                bytes,
                wal_bytes,
                capture_inbox_backlog,
                pack_count,
                pack_bytes,
            }
        }
        Err(e) => StateDbHealth {
            path: None,
            exists: false,
            quick_check: format!("unavailable: {e}"),
            bytes: 0,
            wal_bytes: 0,
            capture_inbox_backlog: 0,
            pack_count: 0,
            pack_bytes: 0,
        },
    };

    // 2. Rebuildable `catalog.db` + writer versions
    let writer_versions: Vec<WriterVersionHealth> = crate::catalog::writer_versions()
        .into_iter()
        .map(|o| {
            let newer = o
                .newest
                .as_deref()
                .and_then(|v| crate::writer_version::compare(v, o.verified))
                == Some(std::cmp::Ordering::Greater);
            WriterVersionHealth {
                harness: o.harness.to_string(),
                verified: o.verified.to_string(),
                newest: o.newest,
                sessions: o.sessions,
                unknown: o.unknown,
                newer_than_verified: newer,
            }
        })
        .collect();

    let cat_path = cv_cache.as_ref().map(|d| d.join("catalog.db"));
    let catalog_db = match cat_path {
        Some(path) => {
            let bytes = file_len_or_zero(&path);
            let wal_bytes = file_len_or_zero(&wal_sidecar(&path));
            let exists = path.is_file();
            let (quick_check, session_rows) = if !exists || bytes == 0 {
                ("absent".to_string(), 0)
            } else {
                match crate::harness::store::sqlite::open_ro(&path) {
                    Ok(reader) => match reader.read_snapshot(|conn, _reads| {
                        let qc = sqlite_quick_check(conn);
                        let rows = conn
                            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))
                            .map(|n| n.max(0) as u64)
                            .unwrap_or(0);
                        Ok((qc, rows))
                    }) {
                        Ok(pair) => pair,
                        Err(e) => (format!("error: {e}"), 0),
                    },
                    Err(e) => (format!("error: {e}"), 0),
                }
            };
            CatalogDbHealth {
                path: Some(path.display().to_string()),
                exists,
                quick_check,
                bytes,
                wal_bytes,
                session_rows,
                writer_versions,
            }
        }
        None => CatalogDbHealth {
            path: None,
            exists: false,
            quick_check: "unavailable".to_string(),
            bytes: 0,
            wal_bytes: 0,
            session_rows: 0,
            writer_versions,
        },
    };

    // 3. Derived search indexes (`tantivy` FTS, `trigram`, `events_columnar` `.clev` segments)
    let search_indexes = SearchIndexesHealth {
        fts: inspect_index_dir(cv_cache.as_ref().map(|d| d.join("tantivy")), false),
        trigram: inspect_index_dir(cv_cache.as_ref().map(|d| d.join("trigram")), false),
        columnar_events: inspect_index_dir(cv_cache.as_ref().map(|d| d.join("events_columnar")), true),
    };

    // 4. Storage class (`IoAdmission`) & free disk space (`statvfs` on `$CLUSTERVISION_HOME`)
    let storage = {
        let target = cv_home
            .clone()
            .or_else(|| cv_cache.clone())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let existing_probe = target
            .ancestors()
            .find(|p| !p.as_os_str().is_empty() && p.exists())
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let (dev, class) = crate::io_admission::inspect_path_device(&existing_probe);
        let policy = crate::io_admission::IoAdmission::global().policy_for(dev, Some(class));
        let class_name = match policy.class {
            crate::io_admission::DeviceClass::Nvme => "nvme",
            crate::io_admission::DeviceClass::RotationalHdd => "hdd",
            crate::io_admission::DeviceClass::Network => "network",
            crate::io_admission::DeviceClass::Unknown => "unknown",
        };
        let (free_bytes, total_bytes) = statvfs_space(&existing_probe);
        StorageHealth {
            path: cv_home.map(|p| p.display().to_string()),
            device_class: class_name.to_string(),
            max_streams: policy.max_streams,
            free_bytes,
            total_bytes,
        }
    };

    // 5. Skipped unreadable harness sources
    let skipped_sources = crate::catalog::skipped_sources()
        .into_iter()
        .map(|(h, s)| SkippedSourceHealth {
            harness: h.as_str().to_string(),
            path: s.path.display().to_string(),
            why: s.why,
        })
        .collect();

    StoreHealthReport {
        sqlite_version: crate::state::sqlite_version().to_string(),
        state_db,
        catalog_db,
        search_indexes,
        storage,
        skipped_sources,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str, body: &str) -> Message {
        let mut m = Message::new(Role::Tool);
        m.content.push(Block::ToolResult {
            tool_use_id: id.into(),
            content: body.to_string().into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        });
        m
    }

    fn call(id: &str, name: &str) -> Message {
        let mut m = Message::new(Role::Assistant);
        m.content.push(Block::ToolUse {
            id: id.into(),
            name: name.into(),
            input: json!({}).into(),
            namespace: None,
        });
        m
    }

    #[test]
    fn unreadable_text_cannot_be_published_as_zero_tokens() {
        let world = crate::test_world("doctor-checked-text");
        let path = world.base.join("payload.json");
        let wire = serde_json::to_vec(&"🌙\n".repeat(8192)).unwrap();
        std::fs::write(&path, &wire).unwrap();
        let retained = crate::json::Json::source(&path, 0, wire.len() as u64)
            .unwrap()
            .with_view(|view| view.source_text())
            .unwrap()
            .unwrap();
        let span = crate::lazy::Text::Span(crate::lazy::Span {
            source: Some(path.clone()),
            offset: 1,
            len: wire.len() as u64 - 2,
            escaped: true,
        });
        std::fs::remove_file(path).unwrap();
        for text in [retained, span] {
            let mut report = Report::default();
            let mut pass = report.session(None);
            let mut message = Message::new(Role::Assistant);
            message.content.push(Block::Text { text });
            assert!(matches!(pass.message(message), Flow::Stop));
            assert!(pass.finish().is_err());
        }
    }

    /// A result that names no tool attributes to its call's tool even when the call streams past
    /// after it (a resumed transcript re-copies history); an id no call carries is unknown.
    #[test]
    fn unnamed_results_resolve_against_the_whole_session() {
        let mut rep = Report::default();
        let mut pass = rep.session(None);
        for m in [
            result("t1", &"x".repeat(70)),
            call("t1", "mcp__s__q"),
            result("t9", "y"),
        ] {
            let _ = pass.message(m);
        }
        pass.finish().unwrap();
        let mcp = &rep.by_tool["mcp__s__q"];
        assert_eq!((mcp.calls, mcp.is_mcp), (1, true));
        assert!(mcp.result_tokens > rep.by_tool["(unknown tool)"].result_tokens);
        assert_eq!(rep.mcp_tool_results(), mcp.result_tokens);
        assert_eq!((rep.sessions, rep.messages), (1, 3));
    }

    #[test]
    fn harness_only_payloads_never_contribute_context_or_tool_attribution() {
        let mut report = Report::default();
        let mut pass = report.session(None);
        for kind in [
            MessageKind::Notice,
            MessageKind::Error,
            MessageKind::Carrier,
            MessageKind::CompactionBoundary,
            MessageKind::Branch,
            MessageKind::ModelChange,
        ] {
            let mut message = Message::of_kind(Role::Assistant, kind, crate::Origin::Harness);
            message.content = vec![
                Block::Text {
                    text: "harness text".into(),
                },
                Block::Thinking {
                    text: "harness thought".into(),
                    signature: Some("signature".into()),
                    encrypted: Some("opaque".into()),
                    redacted: false,
                },
                Block::ToolUse {
                    id: "hidden".into(),
                    name: "mcp__hidden__tool".into(),
                    input: json!({"argument":"hidden"}).into(),
                    namespace: None,
                },
                Block::ToolResult {
                    tool_use_id: "hidden".into(),
                    tool_name: Some("mcp__hidden__tool".into()),
                    content: "harness result".into(),
                    is_error: false,
                    status: None,
                    details: None,
                },
                Block::Image {
                    media_type: None,
                    data_ref: None,
                },
            ];
            assert_eq!(pass.message(message), Flow::Continue);
        }
        pass.finish().unwrap();
        assert_eq!(report.messages, 6);
        assert_eq!(report.conv_total(), 0);
        assert!(report.by_tool.is_empty());
        assert!(report.by_attachment.is_empty());
        let mut pass = report.session(None);
        let mut visible = Message::of_kind(Role::User, MessageKind::InjectedContext, crate::Origin::Harness);
        visible.content.push(Block::Text {
            text: "reminder".into(),
        });
        pass.message(visible);
        pass.finish().unwrap();
        assert!(report.attachments > 0);
    }

    #[test]
    fn active_window_slices_post_compaction_and_subtracts_visible_turn_overhead() {
        let mut rep = Report::default();
        let mut pass = rep.session(None);

        // Pre-compaction heavy bloat: 40k bytes of tool result + 50k prompt usage.
        let mut u1 = Message::new(Role::User);
        u1.id = Some("m1".into());
        u1.content.push(Block::Text {
            text: "initial question".into(),
        });
        let mut a1 = call("t-pre", "Bash");
        a1.id = Some("m2".into());
        a1.parent_id = Some("m1".into());
        a1.usage = Some(crate::ir::Usage {
            input_tokens: Some(50_000),
            ..Default::default()
        });
        let mut tr1 = result("t-pre", &"B".repeat(40_000));
        tr1.id = Some("m3".into());
        tr1.parent_id = Some("m2".into());

        // Compaction boundary + summary seed.
        let mut boundary = Message::of_kind(Role::System, MessageKind::CompactionBoundary, crate::Origin::Harness);
        boundary.id = Some("b1".into());
        boundary.parent_id = Some("m3".into());
        boundary.extra.insert(
            "claude".into(),
            json!({"compactMetadata": {"trigger": "auto", "preTokens": 58000}}),
        );

        let mut summary = Message::of_kind(Role::User, MessageKind::CompactionSummary, crate::Origin::Harness);
        summary.id = Some("s1".into());
        summary.parent_id = Some("b1".into());
        summary.content.push(Block::Text {
            text: "S".repeat(2500).into(),
        });

        // Post-compaction turn 1: user prompt + assistant reply with usage=20_000.
        let mut u2 = Message::new(Role::User);
        u2.id = Some("m4".into());
        u2.parent_id = Some("s1".into());
        u2.content.push(Block::Text {
            text: "U".repeat(500).into(),
        });

        let mut a2 = Message::new(Role::Assistant);
        a2.id = Some("m5".into());
        a2.parent_id = Some("m4".into());
        a2.usage = Some(crate::ir::Usage {
            input_tokens: Some(20_000),
            ..Default::default()
        });
        a2.content.push(Block::Text {
            text: "clean post-compaction answer".into(),
        });

        // Post-compaction turn 2: user follow-up + assistant reply with usage=21_400.
        let mut u3 = Message::new(Role::User);
        u3.id = Some("m6".into());
        u3.parent_id = Some("m5".into());
        u3.content.push(Block::Text {
            text: "next step".into(),
        });

        let mut a3 = Message::new(Role::Assistant);
        a3.id = Some("m7".into());
        a3.parent_id = Some("m6".into());
        a3.usage = Some(crate::ir::Usage {
            input_tokens: Some(21_400),
            ..Default::default()
        });
        a3.content.push(Block::Text { text: "done".into() });

        for m in [u1, a1, tr1, boundary, summary, u2, a2, u3, a3] {
            assert_eq!(pass.message(m), Flow::Continue);
        }
        pass.finish().unwrap();

        // Lifetime history retains the massive pre-compaction Bash tool result.
        assert!(rep.tool_results > 10_000);
        assert_eq!(rep.compactions, 1);
        assert_eq!(rep.startup_ctx, 50_000);

        // Active post-compaction window excludes pre-compaction tool bloat completely!
        let aw = &rep.active_window;
        assert!(aw.from_compaction);
        assert_eq!(aw.tool_results, 0);
        assert!(aw.by_tool.is_empty());
        assert!(aw.compaction_summary > 800);
        assert_eq!(aw.turns, 2);
        assert_eq!(aw.first_prompt_tokens, Some(20_000));
        assert!(aw.reconstructed_visible_turn_tokens > 1_000);
        let expected_overhead = 20_000 - aw.reconstructed_visible_turn_tokens;
        assert_eq!(aw.system_and_tools_overhead, Some(expected_overhead));
        assert!(expected_overhead < 20_000);
        assert_eq!(aw.current_context_tokens, Some(21_400));
        assert_eq!(aw.token_burn_per_turn, 1_400);
    }
}
