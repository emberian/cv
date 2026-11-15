//! `cv watch --kind precompact`: write a live session's compaction summary before its harness
//! asks for it, keep it current as the session grows, and hand it to the harness inside the
//! harness's own compaction. A `cv watch` consumer, not a verb of its own (the 0.19 design record,
//! D4).
//!
//! The core (record, key, commitments, the commit check, profiles, prompts) is
//! `cv_core::precompact`; this module is the thin driver around it: the resident follower
//! (`cv watch --kind precompact`: prepare under a lease, publish ready records), and its actions:
//! one-shot writes (`now`), the commit a harness plugin calls inside its compaction (`commit`),
//! the record of every compaction that plugin saw, cv's or the harness's own (`observe`), the
//! replay check (`check`), the explicit new-session fallback (`swap`), and the measurements
//! (`report`). No proxy sits in the path. Design: `docs/design/PRECOMPACT.md`.

use crate::util::{fmt_int, parse_harness, short_id, usage, Wait};
use anyhow::{Context, Result};
use clap::Subcommand;
use cv_core::ir::{Block, Harness, Message, MessageKind, Role, SessionRef};
use cv_core::precompact::{self as pc, prompt, prompt::Turn, store, Record, Settings, Window};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A summary's output cap. The harness's own summaries on our fleet ran 9-17k tokens.
const SUMMARY_MAX_TOKENS: u32 = 20_000;
/// The extraction pass's output cap: quotes, not prose.
const EXTRACT_MAX_TOKENS: u32 = 12_000;
/// How long one summary call may take before it is abandoned (the session keeps its old record).
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(900);
/// The first retry of a session whose read failed (bytes it had read changed under it, or it
/// could not be read); each further failure doubles it, up to [`RETRY_MAX`]. Cost-only. A
/// session appended to while it is read is not a failure: the read ends where it began, and the
/// appended bytes are the next read's.
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(600);

/// The pause before a session whose summary call failed is tried again: [`RETRY_FIRST`], doubled
/// by each further failure in a row, up to [`RETRY_MAX`]; a written summary clears it. Without it a
/// summary call that fails fast (a refused connection, a 4xx) was started again on the wake its
/// own failure sent, as fast as the loop could turn. Cost-only.
fn summary_pause(failures: u32) -> Duration {
    RETRY_FIRST
        .saturating_mul(1 << failures.saturating_sub(1).min(16))
        .min(RETRY_MAX)
}
/// A lease older than this is another owner's that died: the summary call's own timeout and a
/// minute. Cost-only.
const LEASE_STALE: Duration = Duration::from_secs(900 + 60);
/// Exit code for a miss: the caller falls through to the harness's own compaction.
pub(crate) const MISS_EXIT: i32 = 3;

/// What `cv watch` keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum WatchKind {
    /// Every session snapshotted as IR under the archive home.
    Archive,
    /// A compaction summary of each live session's window, kept current.
    Precompact,
}

/// `cv watch --kind precompact`, as parsed: the follower's scope and thresholds, or one action.
pub(crate) struct Watch {
    pub harness: Option<String>,
    pub cwd: Option<String>,
    pub dry_run: bool,
    pub for_: Option<String>,
    pub knobs: Knobs,
    pub action: Option<PrecompactCmd>,
}

/// The actions of `cv watch --kind precompact`; without one, it follows live sessions and keeps
/// each one's summary current: it starts at a fraction of the harness's auto-compact point and
/// extends by new tokens or when the session goes idle. It wakes on file changes (inotify on
/// Linux), not a timer, and leaves alone a session whose harness precomputes its own compactions.
#[derive(Subcommand)]
pub(crate) enum PrecompactCmd {
    /// Write (or extend) one session's summary now, up to its newest message.
    Now {
        /// Session id, `harness:id`, or a transcript path.
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Print what would be sent and written, call nothing.
        #[arg(long)]
        dry_run: bool,
        /// Summarize the session as it stood when its transcript was this many bytes long: the
        /// window then, up to its newest message (a recorded span, offline).
        #[arg(long, value_name = "BYTES")]
        upto_bytes: Option<u64>,
        /// Print the record as JSON on stdout and write nothing: a fresh generation (no stored
        /// summary or carry is read), no lease, no store.
        #[arg(long)]
        print: bool,
        #[command(flatten)]
        knobs: Box<Knobs>,
    },
    /// The commit, called inside the harness's compaction (Claude Code's `session.compact` plugin
    /// event): the event JSON `{session, trigger, instructions, messages}` on stdin; on a pass,
    /// `{"outcome":"served","summary":…,"keep_from":N}` on stdout, and the harness replaces the
    /// messages before `keep_from` with the summary. A miss exits 3 with the reason on stderr,
    /// and the harness compacts as its own.
    Commit {
        /// Which compaction serves this seat (`native`, `cv`); default: `precompact.serve`.
        #[arg(long, value_name = "SERVE")]
        serve: Option<String>,
        /// Only say whether this compaction would be served now, serving nothing: a plugin asks
        /// this before the harness starts its own background summary, and vetoes that summary
        /// when the answer is yes. Exit 0 yes, exit 3 no, and the reason is not counted as a miss.
        #[arg(long)]
        check: bool,
        /// The harness compacting (default: `claude`).
        #[arg(long)]
        harness: Option<String>,
    },
    /// Record one compaction a harness plugin saw, cv's or the harness's own: a JSON object on
    /// stdin (`{session, trigger, method, ms, tokens_before, tokens_after, kept, summary_bytes,
    /// miss}`), appended to the measurements `report` reads.
    Observe,
    /// Removed with the proxy path: the harness hands cv its compaction in process (`commit`).
    #[command(hide = true)]
    Answer {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<String>,
    },
    /// Check a captured compaction request against a session's fingerprint, with no store and no
    /// model: the replay behind the anchor match rate. Prints one JSON verdict.
    Check {
        /// The captured request (JSON: a body, or `{body, headers}`).
        request: PathBuf,
        /// The session: id, `harness:id`, or a transcript path.
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Read only the transcript's first N bytes (the state when the request was sent).
        #[arg(long, value_name = "BYTES")]
        upto_bytes: Option<u64>,
        /// Cut where the window's fill last stood at or under this many tokens (a precompute
        /// made earlier); default: at the newest message.
        #[arg(long, value_name = "TOKENS")]
        cut_tokens: Option<u64>,
    },
    /// Harness compaction instructions captured from real requests.
    Profile {
        #[command(subcommand)]
        action: ProfileCmd,
    },
    /// The stored summaries.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// What the measurements say: compactions by method (stall, tokens before and after), commits
    /// served and missed by class: the comparison a seat's `serve` setting is chosen by.
    Report {
        #[arg(long)]
        json: bool,
    },
    /// The fallback without a proxy (Claude Code): a NEW session holding the stored summary as
    /// its compaction and the messages after the cut, to resume into. The source is untouched.
    Swap {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ProfileCmd {
    /// Capture the compaction instruction of a request file.
    Import {
        request: PathBuf,
        /// The harness that sent it.
        #[arg(long)]
        harness: String,
    },
    /// List the captured profiles.
    Ls,
}

impl PrecompactCmd {
    /// Whether the action writes a store (refused under `--frozen`).
    pub(crate) fn writes(&self) -> bool {
        matches!(
            self,
            PrecompactCmd::Now {
                dry_run: false,
                print: false,
                ..
            } | PrecompactCmd::Swap { dry_run: false, .. }
                | PrecompactCmd::Profile {
                    action: ProfileCmd::Import { .. }
                }
        )
    }
}

/// The threshold flags; each overrides `[precompact]` in the config, which overrides the named
/// defaults in `cv_core::precompact`.
#[derive(clap::Args, Default, PartialEq)]
pub(crate) struct Knobs {
    /// The harness's auto-compact point in tokens (default: learned from its own records).
    #[arg(long, value_name = "TOKENS")]
    compact_at: Option<u64>,
    /// Start at this fraction of the auto-compact point (default 0.70).
    #[arg(long, value_name = "F")]
    start_fraction: Option<f64>,
    /// Extend after this many new tokens (default 20000).
    #[arg(long, value_name = "TOKENS")]
    extend_tokens: Option<u64>,
    /// Extend when the session has been idle this long (default 45).
    #[arg(long, value_name = "SECS")]
    idle_secs: Option<u64>,
    /// Bytes of each tool result the summary model reads (default 2048).
    #[arg(long, value_name = "BYTES")]
    tool_result_bytes: Option<usize>,
    /// The summary model.
    #[arg(long)]
    model: Option<String>,
    /// The OpenAI-compatible server the summary passes go to (`https://host/v1`).
    #[arg(long, value_name = "URL")]
    base_url: Option<String>,
    /// The environment variable holding that server's key (its name, never the key).
    #[arg(long, value_name = "NAME")]
    api_key_env: Option<String>,
    /// Summary jobs at once (default 2).
    #[arg(long, value_name = "N")]
    jobs: Option<usize>,
    /// Follow sessions touched within this many seconds of startup (default 7200).
    #[arg(long, value_name = "SECS")]
    live_within_secs: Option<u64>,
    /// Also prepare summaries where the harness precomputes its own (to serve cv's on such a
    /// seat, and to compare the two).
    #[arg(long)]
    prepare_native: bool,
    /// A regular expression finding a standing rule's id (its first capture group, else the
    /// whole match) in what the session read; repeat for several. Replaces `rule_patterns`.
    #[arg(long = "rule-pattern", value_name = "REGEX")]
    rule_patterns: Vec<String>,
}

impl Knobs {
    /// Whether any threshold flag was given.
    pub(crate) fn is_set(&self) -> bool {
        *self != Knobs::default()
    }

    fn settings(&self) -> Settings {
        let mut s = cv_core::config::load().precompact.unwrap_or_default();
        if self.compact_at.is_some() {
            s.compact_at = self.compact_at;
        }
        macro_rules! take {
            ($($f:ident),*) => {$(if let Some(v) = self.$f.clone() { s.$f = v; })*};
        }
        take!(
            start_fraction,
            extend_tokens,
            idle_secs,
            tool_result_bytes,
            jobs,
            live_within_secs
        );
        if self.model.is_some() {
            s.model = self.model.clone();
        }
        if self.base_url.is_some() {
            s.base_url = self.base_url.clone();
        }
        if self.api_key_env.is_some() {
            s.api_key_env = self.api_key_env.clone();
        }
        if self.prepare_native {
            s.prepare_native = true;
        }
        if !self.rule_patterns.is_empty() {
            s.rule_patterns = self.rule_patterns.clone();
        }
        s.jobs = s.jobs.max(1);
        s
    }
}

pub(crate) fn cmd_watch(args: Watch) -> Result<()> {
    let Watch {
        harness,
        cwd,
        dry_run,
        for_,
        knobs,
        action,
    } = args;
    let Some(action) = action else {
        return watch(
            parse_harness(&harness)?,
            cwd,
            dry_run,
            for_.as_deref(),
            knobs.settings(),
        );
    };
    if harness.is_some() || cwd.is_some() || dry_run || for_.is_some() || knobs.is_set() {
        return usage(
            "`--harness`, `--cwd`, `--dry-run`, `--for` and the threshold flags before an action belong to \
             the follower; give an action its own flags after its name",
        );
    }
    match action {
        PrecompactCmd::Now {
            id,
            harness,
            dry_run,
            upto_bytes,
            print,
            knobs,
        } => now(
            &id,
            parse_harness(&harness)?,
            Span {
                dry_run,
                upto_bytes,
                print,
            },
            &knobs.settings(),
        ),
        PrecompactCmd::Commit { serve, check, harness } => commit(serve, check, harness),
        PrecompactCmd::Observe => observe(),
        PrecompactCmd::Answer { .. } => usage(
            "`cv watch --kind precompact answer` was removed with the proxy path (the 0.19 design: no \
             proxy in the critical path); a harness plugin calls `cv watch --kind precompact commit` \
             inside its own compaction (integrations/claude-code/cv-compact)",
        ),
        PrecompactCmd::Check {
            request,
            id,
            harness,
            upto_bytes,
            cut_tokens,
        } => check(&request, &id, parse_harness(&harness)?, upto_bytes, cut_tokens),
        PrecompactCmd::Profile { action } => profile(action),
        PrecompactCmd::Ls { json } => ls(json),
        PrecompactCmd::Report { json } => report(json),
        PrecompactCmd::Swap { id, harness, dry_run } => swap(&id, parse_harness(&harness)?, dry_run),
    }
}

// ───────────────────────────── reading a session ─────────────────────────────

fn text_of(t: &cv_core::Text, resolver: &cv_core::Resolver) -> Result<String> {
    let mut s = String::new();
    t.try_for_each_chunk(resolver, 64 * 1024, |part| {
        s.push_str(part);
        Ok(())
    })?;
    Ok(s)
}

/// One message as the summary model reads it: its id first (`[m:ID]`, which the summary cites),
/// prose whole, tool calls and results cut to `tool_bytes`, reasoning left out. `None` for what
/// no window carries (boundaries, notices).
fn render(m: &Message, resolver: &cv_core::Resolver, tool_bytes: usize) -> Result<Option<Turn>> {
    if matches!(
        m.kind,
        MessageKind::CompactionBoundary | MessageKind::SystemPrompt | MessageKind::Notice | MessageKind::ModelChange
    ) {
        return Ok(None);
    }
    let role = if m.role == Role::Assistant { "assistant" } else { "user" };
    // The person's message delivered mid-turn reads as their words, whole, not as the reminder
    // the harness wrapped it in: the extraction quotes it, and a ruling is checked against it.
    if let Some(words) = pc::window::queued_prompt(m) {
        let words = words.trim();
        return Ok((!words.is_empty()).then(|| Turn {
            role,
            content: match &m.id {
                Some(id) => format!("[m:{id}] [sent while the agent was working] {words}"),
                None => format!("[sent while the agent was working] {words}"),
            },
        }));
    }
    let injected = m.kind == MessageKind::InjectedContext;
    let mut out = String::new();
    for b in &m.content {
        let piece = match b {
            Block::Text { text } => {
                let t = text_of(text, resolver)?;
                if injected {
                    prompt::clip(&t, tool_bytes).into_owned()
                } else {
                    t
                }
            }
            Block::Thinking { .. } => continue,
            Block::ToolUse { name, input, .. } => {
                let mut args = String::new();
                cv_core::projection::append_json(&mut args, input)?;
                format!("[tool call: {name} {}]", prompt::clip(&args, tool_bytes))
            }
            Block::ToolResult { content, .. } => {
                format!(
                    "[tool result] {}",
                    prompt::clip(&text_of(content, resolver)?, tool_bytes)
                )
            }
            Block::Image { .. } => "[image]".into(),
            Block::File { path, .. } => format!("[file {}]", path.as_deref().unwrap_or("")),
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&piece);
    }
    let out = out.trim();
    Ok((!out.is_empty()).then(|| Turn {
        role,
        content: match &m.id {
            Some(id) => format!("[m:{id}] {out}"),
            None => out.to_owned(),
        },
    }))
}

/// The compiled `rule_patterns`, once per distinct list; an invalid pattern is left out (the
/// follower says so when it starts).
fn rule_regexes(patterns: &[String]) -> std::sync::Arc<Vec<regex::Regex>> {
    use std::sync::{Arc, Mutex};
    type Cache = Option<(Vec<String>, Arc<Vec<regex::Regex>>)>;
    static CACHE: Mutex<Cache> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((key, compiled)) = cache.as_ref() {
        if key.as_slice() == patterns {
            return compiled.clone();
        }
    }
    let compiled = Arc::new(patterns.iter().filter_map(|p| regex::Regex::new(p).ok()).collect());
    *cache = Some((patterns.to_vec(), Arc::clone(&compiled)));
    compiled
}

/// A session being followed: its window, the rendered span since the record's cut, its record.
struct Live {
    r: SessionRef,
    window: Window,
    span: Vec<(usize, Turn)>,
    record: Option<Record>,
    job: bool,
    primed: bool,
    said: &'static str,
    /// Messages delivered by the watcher, and how many of them a completed read accepted. A read
    /// that fails after delivering some is re-delivered from the accepted position.
    delivered: usize,
    accepted: usize,
    /// Messages of a failed read still to come again, already observed: skipped.
    redelivered: usize,
    /// The window's injected-context messages (index, id): a layer no carried item may quote.
    injected: Vec<(usize, String)>,
    /// The window's replies (index, id): a layer no self or setup fact may quote.
    replies: Vec<(usize, String)>,
    /// The window's summaries the harness wrote itself (index, id): what a standing line quotes.
    summaries: Vec<(usize, String)>,
    /// Standing-rule ids the configured patterns found in what the agent did in the window: its
    /// replies and tool calls, and the refusals its calls drew (index, message id, id).
    found: Vec<(usize, String, String)>,
    /// The window's message ids (index, id): what a narrative cite may name.
    ids: Vec<(usize, String)>,
    /// Summary calls for this session that failed in a row, and when the next may start.
    failures: u32,
    not_before: Option<Instant>,
}

impl Live {
    fn new(r: SessionRef) -> Live {
        Live {
            r,
            window: Window::default(),
            span: Vec::new(),
            record: None,
            job: false,
            primed: false,
            said: "",
            delivered: 0,
            accepted: 0,
            redelivered: 0,
            injected: Vec::new(),
            replies: Vec::new(),
            summaries: Vec::new(),
            found: Vec::new(),
            ids: Vec::new(),
            failures: 0,
            not_before: None,
        }
    }

    /// A message the watcher delivered: whether it is new (`false`: a failed read's message
    /// delivered again, already observed).
    fn take(&mut self) -> bool {
        self.delivered += 1;
        if self.redelivered > 0 {
            self.redelivered -= 1;
            return false;
        }
        true
    }

    /// The watcher reads the session from its start again: everything observed comes again.
    fn restart(&mut self) {
        self.redelivered += self.delivered;
        self.delivered = 0;
        self.accepted = 0;
    }

    /// The watcher's read of this session ended: accepted through what was delivered, or failed,
    /// so it comes again from the last accepted position.
    fn settle(&mut self, accepted: bool) {
        if accepted {
            self.accepted = self.delivered;
        } else {
            self.redelivered += self.delivered - self.accepted;
            self.delivered = self.accepted;
        }
    }

    fn key(&self) -> String {
        format!("{}:{}", self.r.harness.as_str(), self.r.id)
    }

    /// Take the next message. A compaction the harness did ends the window and its record, and
    /// once primed is logged with what cv held for it.
    fn observe(&mut self, m: &Message, settings: &Settings, root: &Path, dry_run: bool) -> Result<()> {
        let resolver = cv_core::Resolver::new(Some(self.r.path.clone()));
        let before = self.window.boundaries.len();
        let index = self.window.messages;
        self.window.observe(m, &resolver)?;
        if self.window.boundaries.len() > before {
            self.span.clear();
            self.injected.clear();
            self.replies.clear();
            self.summaries.clear();
            self.found.clear();
            self.ids.clear();
            let b = self.window.boundaries.last().expect("a boundary was just pushed");
            let record = self.record.take();
            if self.primed {
                let _ = store::event(
                    root,
                    json!({
                        "event": "compaction",
                        "harness": self.r.harness.as_str(),
                        "session": self.r.id,
                        "trigger": b.trigger,
                        "pre_tokens": b.pre_tokens,
                        "post_tokens": b.post_tokens,
                        "duration_ms": b.duration_ms,
                        "native": b.precomputed,
                        "had_record": record.is_some(),
                        "record_cut_tokens": record.as_ref().and_then(|r| r.cut.tokens),
                    }),
                );
                eprintln!(
                    "precompact: {} compacted ({}, {} ms{}{})",
                    short_id(&self.r.id),
                    b.trigger.as_deref().unwrap_or("?"),
                    b.duration_ms.map_or("?".into(), fmt_int),
                    if b.precomputed { ", native precompute" } else { "" },
                    if record.is_some() { ", cv held a summary" } else { "" },
                );
            }
            if let Some(record) = record.filter(|_| !dry_run) {
                // The carried sections outlive the window: the next window's first summary starts
                // from them, so a compaction never re-summarizes what was carried verbatim.
                let _ = store::save_carry(root, self.r.harness.as_str(), &self.r.id, &record.carry);
                let _ = store::remove_record(root, self.r.harness.as_str(), &self.r.id);
            }
            return Ok(());
        }
        if let Some(id) =
            m.id.as_ref()
                .filter(|_| m.kind == MessageKind::InjectedContext && pc::window::queued_prompt(m).is_none())
        {
            self.injected.push((index, id.clone()));
        }
        if let Some(id) = m.id.as_ref().filter(|_| m.kind == MessageKind::Reply) {
            self.replies.push((index, id.clone()));
        }
        if let Some(id) = m.id.as_ref().filter(|_| m.kind == MessageKind::CompactionSummary) {
            self.summaries.push((index, id.clone()));
        }
        if let Some(id) = &m.id {
            self.ids.push((index, id.clone()));
        }
        // A pointer is what the agent did with a rule: named it in its reply or a tool call, or
        // drew a refusal naming it. A rule a hook only injected is the per-turn layer, not a use.
        let patterns = rule_regexes(&settings.rule_patterns);
        if let (Some(id), false) = (&m.id, patterns.is_empty()) {
            let agent = m.role == Role::Assistant;
            for b in &m.content {
                let text = match b {
                    Block::Text { text } if agent => text_of(text, &resolver)?,
                    Block::ToolUse { input, .. } if agent => {
                        let mut args = String::new();
                        cv_core::projection::append_json(&mut args, input)?;
                        args
                    }
                    Block::ToolResult {
                        content,
                        is_error: true,
                        ..
                    } => text_of(content, &resolver)?,
                    _ => continue,
                };
                for re in patterns.iter() {
                    for c in re.captures_iter(&text) {
                        if let Some(rule) = c.get(1).or_else(|| c.get(0)) {
                            self.found.push((index, id.clone(), rule.as_str().to_owned()));
                        }
                    }
                }
            }
        }
        if let Some(turn) = render(m, &resolver, settings.tool_result_bytes)? {
            self.span.push((index, turn));
        }
        Ok(())
    }

    /// After the first full read: adopt a stored record of this same window whose anchors are
    /// still the window's own (a restarted follower picks up where the last one stopped). A record
    /// of an earlier window was written before a compaction no follower saw: with `fold`, its
    /// carried sections become the carry the next summary starts from, as a follower that saw the
    /// compaction would have kept them.
    fn adopt_stored(&mut self, root: &Path, fold: bool) {
        let Ok(Some(record)) = store::load_record(root, self.r.harness.as_str(), &self.r.id) else {
            return;
        };
        if fold && record.window.boundaries < self.window.boundaries.len() {
            if store::save_carry(root, self.r.harness.as_str(), &self.r.id, &record.carry).is_ok() {
                let _ = store::remove_record(root, self.r.harness.as_str(), &self.r.id);
            }
            return;
        }
        let same_window = record.window.boundaries == self.window.boundaries.len()
            && record.cut.index < self.window.messages
            && record.anchors.len() <= self.window.anchors.len()
            && record
                .anchors
                .iter()
                .zip(&self.window.anchors)
                .all(|(a, p)| *a == p.anchor);
        if same_window {
            let cut = record.cut.index;
            self.span.retain(|(i, _)| *i > cut);
            self.record = Some(record);
        }
    }

    fn compact_at(&self, settings: &Settings, learned: &HashMap<(Harness, Option<String>), u64>) -> Option<u64> {
        settings
            .compact_at
            .or_else(|| self.window.compact_point())
            .or_else(|| learned.get(&(self.r.harness, self.window.model.clone())).copied())
    }

    /// The harness precomputes this session's compactions itself: its newest compaction says
    /// so, or its own configuration does. cv then stays idle for it (one method per session).
    fn native(&self) -> bool {
        self.window.native_precompute()
            || cv_core::harness::rootless(self.r.harness).and_then(|a| a.native_precompaction(&self.r)) == Some(true)
    }

    fn idle_secs(&self) -> u64 {
        self.window
            .newest_at
            .map_or(0, |t| (chrono::Utc::now() - t).num_seconds().max(0) as u64)
    }
}

/// What a summary job needs, taken from a [`Live`] when it is due.
struct Job {
    key: String,
    harness: String,
    session: String,
    /// The narrative so far, when extending.
    prior: Option<String>,
    /// The sections carried from the previous generation (this window's record, else the last
    /// window's).
    carry: pc::carry::Carry,
    base: Option<pc::Cut>,
    turns: Vec<Turn>,
    cut: pc::Cut,
    anchors: Vec<pc::Anchor>,
    window: pc::WindowRef,
    profile: pc::Profile,
    /// The ready record's key: the transcript's checkpoint now and the cut message's own id.
    ready_key: Option<pc::Key>,
    prompts: Vec<prompt::Prompt>,
    /// Whose each message of the span is: what a carried item may quote.
    layers: pc::carry::Layers,
    /// Standing-rule ids found in the span: (message id, rule id).
    found: Vec<(String, String)>,
    /// The window's message ids up to the cut: what a narrative cite may name.
    known: std::collections::HashSet<String>,
    compact_at: Option<u64>,
}

impl Job {
    fn take(live: &Live, profile: pc::Profile, extend: bool, compact_at: Option<u64>, root: &Path) -> Option<Job> {
        let (cut, anchors) = live.window.cut_now()?;
        let record = live.record.as_ref().filter(|_| extend);
        let ready_key = cut.id.clone().and_then(|id| pc::Key::take(&live.r.path, id));
        Some(Job {
            key: live.key(),
            harness: live.r.harness.as_str().to_owned(),
            session: live.r.id.clone(),
            prior: record.map(|r| r.narrative.clone()).filter(|n| !n.is_empty()),
            carry: match record {
                Some(r) => r.carry.clone(),
                None => store::load_carry(root, live.r.harness.as_str(), &live.r.id).unwrap_or_default(),
            },
            base: record.map(|r| r.cut.clone()),
            turns: live
                .span
                .iter()
                .filter(|(i, _)| *i <= cut.index)
                .map(|(_, t)| t.clone())
                .collect(),
            prompts: live
                .window
                .prompts
                .iter()
                .filter(|(i, _)| *i <= cut.index)
                .map(|(_, p)| p.clone())
                .collect(),
            layers: pc::carry::Layers {
                prompts: live
                    .window
                    .prompts
                    .iter()
                    .filter(|(i, _)| *i <= cut.index)
                    .filter_map(|(_, p)| p.id.clone())
                    .collect(),
                injected: live
                    .injected
                    .iter()
                    .filter(|(i, _)| *i <= cut.index)
                    .map(|(_, id)| id.clone())
                    .collect(),
                replies: live
                    .replies
                    .iter()
                    .filter(|(i, _)| *i <= cut.index)
                    .map(|(_, id)| id.clone())
                    .collect(),
                summaries: live
                    .summaries
                    .iter()
                    .filter(|(i, _)| *i <= cut.index)
                    .map(|(_, id)| id.clone())
                    .collect(),
            },
            found: live
                .found
                .iter()
                .filter(|(i, _, _)| *i <= cut.index && record.is_none_or(|r| *i > r.cut.index))
                .map(|(_, m, id)| (m.clone(), id.clone()))
                .collect(),
            known: live
                .ids
                .iter()
                .filter(|(i, _)| *i <= cut.index)
                .map(|(_, id)| id.clone())
                .collect(),
            cut,
            anchors,
            window: pc::WindowRef {
                boundaries: live.window.boundaries.len(),
                start: live.window.start,
            },
            profile,
            ready_key,
            compact_at,
        })
    }

    /// The context generation the job prepares: what its lease names.
    fn generation(&self) -> String {
        format!(
            "{}:{}:{}",
            self.window.boundaries,
            self.cut.index,
            self.cut.id.as_deref().unwrap_or("")
        )
    }

    fn prompt_bytes(&self) -> usize {
        self.turns.iter().map(|t| t.content.len()).sum::<usize>() + self.prior.as_ref().map_or(0, String::len)
    }

    /// The two passes and the record: the narrative from the summary model, then the carried
    /// sections from a separate extraction pass whose quotes are kept only where the message
    /// they cite holds them, merged into the previous generation's; identifiers read by shape.
    fn run(self, settings: &Settings) -> Result<(Record, f64, crate::llm::Usage)> {
        let started = Instant::now();
        let lines = {
            let turns: Vec<(&str, String, String)> = self
                .turns
                .iter()
                .filter_map(|t| {
                    let (id, text) = t.content.strip_prefix("[m:")?.split_once("] ")?;
                    (!text.starts_with(pc::SERVED_PREAMBLE)).then(|| (t.role, id.to_owned(), text.to_owned()))
                })
                .collect();
            pc::carry::lines(&turns, self.window.boundaries)
        };
        // Each rendered turn opens with its message id: the texts the quotes are checked against.
        // A summary cv served is left out: it holds every carried quote, so nothing would ever
        // read as unused, and what it says is already carried.
        let texts: Vec<(String, String)> = self
            .turns
            .iter()
            .filter_map(|t| {
                let rest = t.content.strip_prefix("[m:")?;
                let (id, text) = rest.split_once("] ")?;
                Some((id.to_owned(), text.to_owned()))
            })
            .filter(|(_, text)| !text.starts_with(pc::SERVED_PREAMBLE))
            .collect();
        let extract_turns = self.turns.clone();
        let turns = prompt::write(self.prior.as_deref(), self.turns, &self.profile.text);
        let route = crate::llm::Route {
            model: settings.model.as_deref(),
            base_url: settings.base_url.as_deref(),
            api_key_env: settings.api_key_env.as_deref(),
        };
        let written = crate::llm::complete(&route, &turns, SUMMARY_MAX_TOKENS, SUMMARY_TIMEOUT)?;
        let (narrative, model) = (written.text, written.model);
        let mut used = written.usage;
        anyhow::ensure!(!narrative.trim().is_empty(), "the summary model returned no text");
        let mut bounds = vec![format!(
            "tool calls and results cut to {} bytes each",
            settings.tool_result_bytes
        )];
        let (narrative, cites) = pc::carry::cites(&narrative, &self.known);
        if cites.completed + cites.unresolved > 0 {
            bounds.push(format!(
                "citations: {} kept, {} completed from a unique prefix, {} naming no message of the window removed",
                cites.kept, cites.completed, cites.unresolved
            ));
        }
        let extracted = crate::llm::complete(
            &crate::llm::Route {
                model: Some(model.as_str()),
                ..route
            },
            &prompt::extract(extract_turns, &self.carry.listing()),
            EXTRACT_MAX_TOKENS,
            SUMMARY_TIMEOUT,
        );
        let (items, marks, mut dropped) = match &extracted {
            Ok(answer) => {
                used.input += answer.usage.input;
                used.output += answer.usage.output;
                pc::carry::parse(&answer.text)
            }
            Err(e) => {
                bounds.push(format!(
                    "the extraction pass failed, the carry is the previous one: {e:#}"
                ));
                Default::default()
            }
        };
        let by_id: HashMap<String, String> = texts.iter().cloned().collect();
        let items = pc::carry::verify(items, &by_id, &self.layers, &mut dropped);
        let mut marks = pc::carry::verify_marks(marks, &self.carry, &by_id, &self.layers, &mut dropped);
        marks.used.extend(pc::carry::said_again(&self.carry, &texts));
        if dropped != pc::carry::Dropped::default() {
            bounds.push(format!(
                "extraction: dropped {} quote(s) not found verbatim in the message they cite, {} from a layer \
                 cv does not re-introduce, {} shaped like output lines, {} marks on no carried item, {} malformed",
                dropped.unverified, dropped.outside_layer, dropped.noise, dropped.marks, dropped.malformed
            ));
        }
        let standing = |items: &[pc::carry::Item]| {
            items
                .iter()
                .filter(|i| i.section == pc::carry::Section::Standing)
                .count()
        };
        let offered = standing(&items) + standing(&self.carry.items);
        let carry = self.carry.merge(
            pc::carry::Span {
                items,
                ids: pc::carry::ids(&texts),
                lines,
                pointers: pc::carry::pointers(&self.found),
                marks,
                window: self.window.boundaries,
            },
            &pc::carry::Limits {
                ids: settings.carry_ids,
                pointers: settings.carry_pointers,
                retire_after: settings.retire_after,
                standing_bytes: settings.standing_bytes,
                lines_bytes: settings.lines_bytes,
            },
        );
        let left_out = offered.saturating_sub(standing(&carry.items));
        if left_out > 0 {
            bounds.push(format!(
                "{left_out} line(s) of the harness's own summary left out: past {} bytes, retired or repeated",
                settings.standing_bytes
            ));
        }
        let text = format!(
            "{}{}{}",
            narrative.trim_end(),
            carry.render(&self.session),
            prompt::owner_words(&self.prompts, settings.owner_words_bytes, &self.session)
        );
        let record = Record {
            v: pc::RECORD_VERSION,
            harness: self.harness,
            key: self.ready_key,
            session: self.session,
            window: self.window,
            cut: self.cut,
            anchors: self.anchors,
            profile: self.profile.id,
            model,
            created: chrono::Utc::now(),
            base: self.base,
            compact_at: self.compact_at,
            bounds,
            unresolved_cites: cites.unresolved,
            narrative,
            carry,
            text,
        };
        Ok((record, started.elapsed().as_secs_f64(), used))
    }
}

/// The instruction summaries are written to: the captured profile `precompact.profile` names,
/// else cv's own ([`prompt::CV_INSTRUCTION`]).
fn profile_for(root: &Path, settings: &Settings, harness: &str) -> Result<pc::Profile> {
    match &settings.profile {
        Some(id) => store::profiles(root)?
            .into_iter()
            .find(|p| &p.id == id)
            .with_context(|| {
                format!(
                    "precompact.profile names {id}, which is not captured (`cv watch --kind precompact profile ls`)"
                )
            }),
        None => Ok(pc::Profile {
            id: pc::profile_id(prompt::CV_INSTRUCTION),
            harness: harness.to_owned(),
            client: None,
            captured: chrono::Utc::now(),
            text: prompt::CV_INSTRUCTION.to_owned(),
        }),
    }
}

fn written_event(record: &Record, secs: f64, prompt_bytes: usize, span_turns: usize, used: crate::llm::Usage) -> Value {
    json!({
        "event": "written",
        "model": record.model,
        "input_tokens": used.input,
        "output_tokens": used.output,
        "lines": record.carry.lines.len(),
        "harness": record.harness,
        "session": record.session,
        "cut_index": record.cut.index,
        "cut_tokens": record.cut.tokens,
        "base_tokens": record.base.as_ref().and_then(|b| b.tokens),
        "anchors": record.anchors.len(),
        "span_turns": span_turns,
        "prompt_bytes": prompt_bytes,
        "text_bytes": record.text.len(),
        // The selection, for scoring what was chosen: each carried item and pointer, and why.
        "carried": record.carry.items.iter().map(|i| json!({"section": i.section, "m": i.m,
                    "quote_bytes": i.quote.len(), "why": i.why})).collect::<Vec<_>>(),
        "pointers": record.carry.pointers.iter().map(|p| json!({"id": p.id, "last": p.last,
                    "hits": p.hits, "why": p.why})).collect::<Vec<_>>(),
        "ids": record.carry.ids.len(),
        "bounds": record.bounds,
        "model": record.model,
        "secs": (secs * 10.0).round() / 10.0,
    })
}

// ───────────────────────────── watch ─────────────────────────────

/// How often a follower on a home with no session yet looks for the first one.
const EMPTY_HOME_RESCAN: Duration = Duration::from_secs(10);

/// The directories to watch so a change to any followed session, or a new session beside them,
/// wakes the loop: every directory from a store root down to each session's parent. A new
/// directory at a depth where sessions live (or above it) is watched when it appears.
struct Dirs {
    /// Store root → the depth (in path components below it) of a session's parent directory.
    roots: Vec<(PathBuf, usize)>,
}

impl Dirs {
    /// The roots `refs` live under, in the harnesses they name.
    fn of(refs: &[SessionRef]) -> Dirs {
        let mut harnesses: Vec<Harness> = refs.iter().map(|r| r.harness).collect();
        harnesses.sort_by_key(|h| h.as_str());
        harnesses.dedup();
        Dirs::new(&harnesses, refs)
    }

    fn new(harnesses: &[Harness], refs: &[SessionRef]) -> Dirs {
        let mut roots = Vec::new();
        for h in harnesses {
            // The adapter as discovery configures it: its roots are where sessions are found.
            let Some(adapter) = cv_core::harness::for_harness(*h) else {
                continue;
            };
            for root in adapter.roots() {
                let depth = refs
                    .iter()
                    .filter_map(|r| r.path.parent()?.strip_prefix(&root).ok())
                    .map(|rel| rel.components().count())
                    .max();
                if let Some(depth) = depth {
                    roots.push((root, depth));
                }
            }
        }
        Dirs { roots }
    }

    /// The root `dir` lies under and its depth below it.
    fn place(&self, dir: &Path) -> Option<(&Path, usize, usize)> {
        self.roots.iter().find_map(|(root, depth)| {
            let rel = dir.strip_prefix(root).ok()?;
            Some((root.as_path(), rel.components().count(), *depth))
        })
    }

    /// Watch every directory from each root down to where sessions live, so a new session in a
    /// project with none followed yet wakes the loop. Bounded by the depth sessions were found at.
    fn watch_all(&self, wake: &mut cv_core::watch::wake::Wake) {
        for (root, depth) in &self.roots {
            let mut level = vec![root.clone()];
            for at in 0..=*depth {
                let mut next = Vec::new();
                for dir in &level {
                    let _ = wake.watch(dir);
                    if at < *depth {
                        let Ok(rd) = std::fs::read_dir(dir) else { continue };
                        next.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
                    }
                }
                level = next;
            }
        }
    }

    /// Watch the chain from `r`'s root to its parent.
    fn watch_session(&self, wake: &mut cv_core::watch::wake::Wake, r: &SessionRef) {
        let Some(parent) = r.path.parent() else { return };
        let Some((root, _, _)) = self.place(parent) else {
            let _ = wake.watch(parent);
            return;
        };
        let mut dir = parent.to_path_buf();
        loop {
            let _ = wake.watch(&dir);
            if dir == root {
                break;
            }
            if !dir.pop() {
                break;
            }
        }
    }
}

fn watch(
    harness: Option<Harness>,
    cwd: Option<String>,
    dry_run: bool,
    for_: Option<&str>,
    settings: Settings,
) -> Result<()> {
    use cv_core::watch::{wake::Wake, Filter, Watcher};
    use std::cell::RefCell;
    use std::rc::Rc;

    let root = store::root()?;
    let wait = Wait::resolve(true, for_)?;
    for p in &settings.rule_patterns {
        if let Err(e) = regex::Regex::new(p) {
            eprintln!("precompact: rule pattern {p:?} is left out: {e}");
        }
    }
    let filter = Filter {
        harness,
        cwd_contains: cwd,
    };
    let started = Instant::now();
    // Every session of the harnesses in scope places the store roots, so a filter that matches
    // nothing yet (a new project's `--cwd`) still watches for the session that will.
    let all: Vec<SessionRef> = cv_core::sessions()
        .into_iter()
        .filter(|r| filter.harness.is_none_or(|h| h == r.harness))
        .collect();
    let live_within = chrono::Duration::seconds(i64::try_from(settings.live_within_secs).unwrap_or(i64::MAX));
    let recent = |r: &SessionRef| {
        r.updated_at
            .or(r.created_at)
            .is_some_and(|t| chrono::Utc::now() - t <= live_within)
    };
    let mut dirs = Dirs::of(&all);
    let mut followed: Vec<SessionRef> = Vec::new();
    for r in all.iter().filter(|r| filter.matches(r) && recent(r)) {
        // One session reached through linked project directories is followed once.
        if !followed.iter().any(|f| f.harness == r.harness && f.id == r.id) {
            followed.push(r.clone());
        }
    }
    drop(all);
    let follow = Rc::new(RefCell::new(followed));
    let mut wake = Wake::new(Duration::from_secs(3))?;
    for r in follow.borrow().iter() {
        dirs.watch_session(&mut wake, r);
    }
    dirs.watch_all(&mut wake);
    let mut watcher = Watcher::with_discovery(filter.clone(), true, {
        let follow = follow.clone();
        move || follow.borrow().clone()
    });
    eprintln!(
        "cv watch --kind precompact: following {} live session(s) (touched within {}s), watching {} director{}{}",
        follow.borrow().len(),
        settings.live_within_secs,
        wake.watched(),
        if wake.watched() == 1 { "y" } else { "ies" },
        if dry_run {
            "; dry run: no model calls, no records"
        } else {
            ""
        },
    );

    let mut lives: HashMap<String, Live> = HashMap::new();
    let mut learned: HashMap<(Harness, Option<String>), u64> = HashMap::new();
    let (tx, rx) = std::sync::mpsc::channel::<(String, Result<(Record, f64, crate::llm::Usage)>, usize, usize)>();
    let waker = wake.waker();
    let mut running = 0usize;
    let mut first = true;
    // Sessions waiting out a retry after a failed read: when they are followed again.
    let mut held: HashMap<String, (SessionRef, Instant, u32)> = HashMap::new();
    let mut retries: HashMap<String, u32> = HashMap::new();
    let mut looked_up: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // Whether the last wake can have changed a followed session: a write elsewhere in a watched
    // directory (another project's session) reads nothing.
    let mut poll_now = true;
    loop {
        // Sessions whose retry is due are followed again.
        let now = Instant::now();
        let due: Vec<String> = held
            .iter()
            .filter(|(_, h)| h.1 <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for k in due {
            if let Some((r, _, _)) = held.remove(&k) {
                looked_up.remove(&r.path);
                follow.borrow_mut().push(r);
                poll_now = true;
            }
        }
        // Read what changed, one message at a time. A session first seen is read whole in one
        // pass (its window and the compactions that came before); after that only what was
        // appended. A batched poll re-read a session from its last checkpoint to EOF once per
        // batch: quadratic on a long session, and a live fleet never finished priming.
        if poll_now {
            loop {
                let mut errors = Vec::new();
                let mut failed: std::collections::HashSet<String> = std::collections::HashSet::new();
                let mut touched: Vec<String> = Vec::new();
                let polled = watcher.try_poll_messages(
                    || Ok(()),
                    |kind, r, m| {
                        let key = format!("{}:{}", r.harness.as_str(), r.id);
                        let live = lives.entry(key.clone()).or_insert_with(|| Live::new(r.clone()));
                        live.r = r.clone();
                        if touched.last() != Some(&key) {
                            touched.push(key.clone());
                            // The watcher forgets a session that left its discovery (a retry wait, or
                            // idle past the live window) and reads it from the start when it is back.
                            if kind == cv_core::watch::EventKind::New {
                                live.restart();
                            }
                        }
                        if !live.take() || failed.contains(&key) {
                            return Ok(cv_core::Flow::Continue);
                        }
                        if let Err(e) = live.observe(&m, &settings, &root, dry_run) {
                            errors.push(format!("{}: {e:#}", short_id(&r.id)));
                            failed.insert(key);
                        }
                        Ok(cv_core::Flow::Continue)
                    },
                );
                for e in errors {
                    eprintln!("precompact: {e}");
                }
                // A read stops at the first session that fails; every session read before it was
                // accepted. The failed one is re-delivered from its accepted position, so what was
                // observed of it is skipped then, and it waits out a doubling retry.
                let why = polled.as_ref().err().map(|e| format!("{e:#}"));
                for key in &touched {
                    let Some(live) = lives.get_mut(key) else { continue };
                    let path = live.r.path.display().to_string();
                    let read = !why.as_deref().is_some_and(|w| w.contains(&path));
                    live.settle(read);
                    if read {
                        retries.remove(key);
                    }
                }
                match (polled, why) {
                    (Ok(true), _) => continue,
                    (Ok(false), _) => break,
                    (Err(_), Some(why)) => {
                        let mut follow = follow.borrow_mut();
                        let mut gone: Vec<SessionRef> = follow
                            .iter()
                            .filter(|r| why.contains(&r.path.display().to_string()))
                            .cloned()
                            .collect();
                        gone.dedup_by(|a, b| a.harness == b.harness && a.id == b.id);
                        if gone.is_empty() {
                            eprintln!("precompact: {why}");
                            break;
                        }
                        // Every path to the session waits, not only the one that failed: the same
                        // transcript under a linked project directory is the same read.
                        follow.retain(|r| !gone.iter().any(|g| g.harness == r.harness && g.id == r.id));
                        for r in gone {
                            let key = format!("{}:{}", r.harness.as_str(), r.id);
                            let n = retries.entry(key.clone()).or_insert(0);
                            *n += 1;
                            let wait = RETRY_FIRST.saturating_mul(1 << (*n - 1).min(16)).min(RETRY_MAX);
                            eprintln!("precompact: {}: {why}; retried in {}s", short_id(&r.id), wait.as_secs());
                            held.insert(key, (r, Instant::now() + wait, *n));
                        }
                    }
                    (Err(_), None) => unreachable!("an error has its text"),
                }
            }
        }
        for live in lives.values_mut() {
            if !live.primed {
                live.primed = true;
                if !dry_run {
                    live.adopt_stored(&root, true);
                }
            }
            if let Some(point) = live.window.compact_point() {
                let slot = learned
                    .entry((live.r.harness, live.window.model.clone()))
                    .or_insert(point);
                *slot = (*slot).min(point);
            }
        }
        if first {
            first = false;
            eprintln!(
                "cv watch --kind precompact: primed {} session(s) in {:.1}s",
                lives.len(),
                started.elapsed().as_secs_f64()
            );
        }

        // Finished jobs.
        while let Ok((key, outcome, prompt_bytes, span_turns)) = rx.try_recv() {
            running -= 1;
            let Some(live) = lives.get_mut(&key) else { continue };
            live.job = false;
            store::release_lease(&root, live.r.harness.as_str(), &live.r.id);
            match outcome {
                Ok((record, secs, used)) => {
                    let same_window = record.window.boundaries == live.window.boundaries.len();
                    let _ = store::event(&root, written_event(&record, secs, prompt_bytes, span_turns, used));
                    if same_window {
                        if let Err(e) = store::save_record(&root, &record) {
                            eprintln!("precompact: saving {}: {e:#}", short_id(&record.session));
                            continue;
                        }
                        eprintln!(
                            "precompact: {} summary to msg {} ({} tokens) in {:.1}s",
                            short_id(&record.session),
                            record.cut.index,
                            record.cut.tokens.map_or("?".into(), fmt_int),
                            secs
                        );
                        let cut = record.cut.index;
                        live.span.retain(|(i, _)| *i > cut);
                        live.record = Some(record);
                    }
                    live.failures = 0;
                    live.not_before = None;
                }
                Err(e) => {
                    live.failures = live.failures.saturating_add(1);
                    let pause = summary_pause(live.failures);
                    live.not_before = Some(Instant::now() + pause);
                    eprintln!(
                        "precompact: {} summary failed: {e:#}; tried again in {}s",
                        short_id(&live.r.id),
                        pause.as_secs()
                    );
                    let _ = store::event(
                        &root,
                        json!({"event": "failed", "harness": live.r.harness.as_str(), "session": live.r.id, "error": format!("{e:#}"),
                               "failures": live.failures, "retry_s": pause.as_secs()}),
                    );
                }
            }
        }

        // Decide, and start what is due.
        let mut next_idle: Option<u64> = None;
        // The soonest end of a pause after a failed summary: the loop wakes for it.
        let mut paused: Option<Instant> = None;
        for live in lives.values_mut() {
            if live.job {
                continue;
            }
            if let Some(at) = live.not_before {
                if at > Instant::now() {
                    paused = Some(paused.map_or(at, |p| p.min(at)));
                    continue;
                }
                live.not_before = None;
            }
            if live.native() && !settings.prepare_native {
                if live.said != "native" {
                    live.said = "native";
                    eprintln!(
                        "precompact: {}'s harness precomputes its own compactions; leaving it alone",
                        short_id(&live.r.id)
                    );
                }
                continue;
            }
            let idle = live.idle_secs();
            let compact_at = live.compact_at(&settings, &learned);
            let due = pc::due(
                &settings,
                &live.window,
                compact_at,
                live.record.as_ref().map(|r| &r.cut),
                idle,
            );
            let extend = match due {
                pc::Due::Wait(why) => {
                    if why == "waiting for more tokens or for the session to go idle" {
                        let left = settings.idle_secs.saturating_sub(idle);
                        next_idle = Some(next_idle.map_or(left, |n| n.min(left)));
                    }
                    if live.said != why
                        && live.primed
                        && why != "below the start threshold"
                        && why != "no usage record yet"
                    {
                        live.said = why;
                    }
                    continue;
                }
                pc::Due::Start => false,
                pc::Due::Extend => true,
            };
            if running >= settings.jobs {
                next_idle = Some(1);
                continue;
            }
            let profile = match profile_for(&root, &settings, live.r.harness.as_str()) {
                Ok(p) => p,
                Err(e) => {
                    if live.said != "no profile" {
                        live.said = "no profile";
                        eprintln!("precompact: {} is due, but {e:#}", short_id(&live.r.id));
                    }
                    continue;
                }
            };
            let Some(job) = Job::take(live, profile, extend, compact_at, &root) else {
                continue;
            };
            let (prompt_bytes, span_turns) = (job.prompt_bytes(), job.turns.len());
            if dry_run {
                let _ = store::event(
                    &root,
                    json!({"event": "due", "dry_run": true, "harness": job.harness, "session": job.session,
                           "extend": extend, "cut_index": job.cut.index, "cut_tokens": job.cut.tokens,
                           "compact_at": compact_at, "span_turns": span_turns, "prompt_bytes": prompt_bytes}),
                );
                eprintln!(
                    "precompact: would {} {} to msg {} ({} tokens of {} compact point; {} turns, {} bytes)",
                    if extend { "extend" } else { "start" },
                    short_id(&job.session),
                    job.cut.index,
                    job.cut.tokens.map_or("?".into(), fmt_int),
                    compact_at.map_or("?".into(), fmt_int),
                    span_turns,
                    fmt_int(prompt_bytes as u64)
                );
                // Stand in for the record so the dry run's cadence matches a real one.
                live.record = Some(Record {
                    v: pc::RECORD_VERSION,
                    harness: job.harness,
                    session: job.session,
                    key: None,
                    window: job.window,
                    cut: job.cut.clone(),
                    anchors: Vec::new(),
                    profile: job.profile.id,
                    model: String::new(),
                    created: chrono::Utc::now(),
                    base: None,
                    compact_at: None,
                    bounds: Vec::new(),
                    unresolved_cites: 0,
                    narrative: String::new(),
                    carry: Default::default(),
                    text: String::new(),
                });
                let cut = job.cut.index;
                live.span.retain(|(i, _)| *i > cut);
                continue;
            }
            // Prepare: one owner per context generation on this host. Another follower (or a
            // `now`) holding a live lease on the session is left to it.
            match store::take_lease(&root, &job.harness, &job.session, &job.generation(), LEASE_STALE) {
                Ok(true) => {}
                Ok(false) => {
                    if live.said != "leased" {
                        live.said = "leased";
                        eprintln!(
                            "precompact: {} is being prepared by another owner; leaving it to it",
                            short_id(&job.session)
                        );
                    }
                    continue;
                }
                Err(e) => {
                    eprintln!("precompact: {}: no lease: {e:#}", short_id(&job.session));
                    continue;
                }
            }
            live.job = true;
            running += 1;
            let (tx, waker, settings) = (tx.clone(), waker.clone(), settings.clone());
            let env = cv_core::env::Env::current();
            std::thread::spawn(move || {
                let _scope = cv_core::env::Env::reenter(env);
                let key = job.key.clone();
                let outcome = job.run(&settings);
                let _ = tx.send((key, outcome, prompt_bytes, span_turns));
                waker.wake();
            });
        }

        if wait.expired() {
            return Ok(());
        }
        let timeout = match (&wait, next_idle) {
            (Wait::Until(Some((deadline, _))), idle) => {
                let left = deadline.saturating_duration_since(Instant::now());
                Some(idle.map_or(left, |s| left.min(Duration::from_secs(s.max(1)))))
            }
            (_, idle) => idle.map(|s| Duration::from_secs(s.max(1))),
        };
        let retry = held
            .values()
            .map(|h| {
                h.1.saturating_duration_since(Instant::now())
                    .max(Duration::from_secs(1))
            })
            .min();
        let timeout = match (timeout, retry) {
            (Some(t), Some(r)) => Some(t.min(r)),
            (t, r) => t.or(r),
        };
        let paused = paused.map(|at| at.saturating_duration_since(Instant::now()).max(Duration::from_secs(1)));
        let timeout = match (timeout, paused) {
            (Some(t), Some(p)) => Some(t.min(p)),
            (t, p) => t.or(p),
        };
        // A home with no session yet places no root, so nothing would ever wake the loop: look
        // again on the fallback tick until the first session places them.
        let unplaced = dirs.roots.is_empty();
        let timeout = if unplaced {
            Some(timeout.map_or(EMPTY_HOME_RESCAN, |t| t.min(EMPTY_HOME_RESCAN)))
        } else {
            timeout
        };
        let woke = wake.wait(timeout)?;
        // A new directory where sessions live is watched; a new file in one may be a new session.
        // A file is looked up once: a session cv does not follow (filtered out, waiting a retry,
        // or the same transcript under a linked directory) is a write in a watched directory on
        // every append, and must not cost a discovery each time.
        let mut rediscover = woke.overflow;
        if unplaced {
            let all: Vec<SessionRef> = cv_core::sessions()
                .into_iter()
                .filter(|r| filter.harness.is_none_or(|h| h == r.harness))
                .collect();
            dirs = Dirs::of(&all);
            if !dirs.roots.is_empty() {
                dirs.watch_all(&mut wake);
                rediscover = true;
            }
        }
        let known: std::collections::HashSet<PathBuf> = follow.borrow().iter().map(|r| r.path.clone()).collect();
        let mut new_files = Vec::new();
        for path in &woke.paths {
            if known.contains(path) || looked_up.contains(path) {
                continue;
            }
            let Some(parent) = path.parent() else { continue };
            match dirs.place(parent) {
                Some((_, at, depth)) if path.is_dir() && at < depth => {
                    let _ = wake.watch(path);
                    rediscover = true;
                }
                Some((_, at, depth)) if at == depth && path.is_file() => {
                    new_files.push(path.clone());
                    rediscover = true;
                }
                _ => {}
            }
        }
        looked_up.extend(new_files);
        poll_now = woke.paths.is_empty() || rediscover || woke.paths.iter().any(|p| known.contains(p));
        if rediscover {
            let fresh: Vec<SessionRef> = cv_core::sessions()
                .into_iter()
                .filter(|r| filter.matches(r) && recent(r))
                .collect();
            let mut follow = follow.borrow_mut();
            let added: Vec<SessionRef> = fresh
                .into_iter()
                .filter(|r| !follow.iter().any(|f| f.harness == r.harness && f.id == r.id))
                .filter(|r| !held.contains_key(&format!("{}:{}", r.harness.as_str(), r.id)))
                .collect();
            for r in &added {
                dirs.watch_session(&mut wake, r);
                eprintln!("precompact: following new session {}", short_id(&r.id));
            }
            follow.extend(added);
            // A followed file is known by the follow list; one let go later is looked up again.
            looked_up.retain(|p| !follow.iter().any(|r| &r.path == p));
        }
        // Sessions idle past the live window are no longer read. What cv holds for one stays, so
        // a session that comes back is read on from where it was: the watcher keeps its position.
        let stale: Vec<String> = lives
            .iter()
            .filter(|(_, l)| !l.job && l.primed && l.idle_secs() > settings.live_within_secs)
            .map(|(k, _)| k.clone())
            .collect();
        if !stale.is_empty() {
            follow
                .borrow_mut()
                .retain(|r| !stale.contains(&format!("{}:{}", r.harness.as_str(), r.id)));
        }
    }
}

// ───────────────────────────── now / check ─────────────────────────────

/// Read `r` (its first `upto_bytes` bytes when given) into a [`Live`].
fn read_live(
    r: &SessionRef,
    adapter: &dyn cv_core::Adapter,
    upto_bytes: Option<u64>,
    settings: &Settings,
    root: &Path,
    mut each: impl FnMut(&Live, &Message),
) -> Result<Live> {
    let mut live = Live::new(r.clone());
    let mut error = None;
    let mut sink = |m: Message| {
        if let Err(e) = live.observe(&m, settings, root, true) {
            error = Some(e);
            return cv_core::Flow::Stop;
        }
        each(&live, &m);
        cv_core::Flow::Continue
    };
    match upto_bytes {
        Some(len) => {
            adapter
                .transcript(
                    r,
                    &cv_core::Demand::RICH,
                    cv_core::stream::Start::Prefix(len),
                    &mut sink,
                )?
                .with_context(|| format!("{} cannot read a byte prefix of {}", r.harness, r.path.display()))?;
        }
        None => {
            adapter.try_stream(r, &cv_core::Demand::RICH, &mut sink)?;
        }
    }
    if let Some(e) = error {
        return Err(e);
    }
    live.primed = true;
    Ok(live)
}

/// How `now` reads and where it writes.
struct Span {
    dry_run: bool,
    upto_bytes: Option<u64>,
    print: bool,
}

/// Write one session's summary now: the window as it stands (or stood at `upto_bytes`). With
/// `print`, the record goes to stdout as JSON and nothing is read from or written to the store:
/// the seam an offline evaluation summarizes any recorded span through.
fn now(id: &str, harness: Option<Harness>, span: Span, settings: &Settings) -> Result<()> {
    let (r, adapter) = crate::cmd::distill::resolve_source(id, harness)?;
    let root = store::root()?;
    let mut live = read_live(&r, adapter.as_ref(), span.upto_bytes, settings, &root, |_, _| {})?;
    if live.native() {
        eprintln!(
            "precompact: note: {}'s harness precomputes its own compactions",
            short_id(&r.id)
        );
    }
    if !span.print {
        live.adopt_stored(&root, !span.dry_run);
    }
    let profile = profile_for(&root, settings, r.harness.as_str())?;
    let extend = live.record.is_some();
    let compact_at = live.compact_at(settings, &HashMap::new());
    let mut job = Job::take(&live, profile, extend, compact_at, &root).context("the session's window is empty")?;
    if span.print {
        job.carry = Default::default();
    }
    eprintln!(
        "precompact: {} {} to msg {} ({} tokens): {} turns, {} bytes",
        if extend { "extending" } else { "writing" },
        short_id(&r.id),
        job.cut.index,
        job.cut.tokens.map_or("?".into(), fmt_int),
        job.turns.len(),
        fmt_int(job.prompt_bytes() as u64)
    );
    if span.dry_run {
        return Ok(());
    }
    if span.print {
        let (record, secs, _) = job.run(settings)?;
        println!("{}", serde_json::to_string(&record)?);
        eprintln!("precompact: wrote the summary in {secs:.1}s");
        return Ok(());
    }
    if !store::take_lease(&root, &job.harness, &job.session, &job.generation(), LEASE_STALE)? {
        return usage(format!(
            "{} is being prepared by another owner (its lease is under {})",
            short_id(&r.id),
            root.join("leases").display()
        ));
    }
    let (prompt_bytes, span_turns) = (job.prompt_bytes(), job.turns.len());
    let (harness, session) = (job.harness.clone(), job.session.clone());
    let outcome = job.run(settings);
    store::release_lease(&root, &harness, &session);
    let (record, secs, used) = outcome?;
    store::event(&root, written_event(&record, secs, prompt_bytes, span_turns, used))?;
    let path = store::save_record(&root, &record)?;
    eprintln!("precompact: wrote {} in {secs:.1}s", path.display());
    Ok(())
}

fn read_request(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))
}

fn check(
    request: &Path,
    id: &str,
    harness: Option<Harness>,
    upto_bytes: Option<u64>,
    cut_tokens: Option<u64>,
) -> Result<()> {
    let input = read_request(request)?;
    let view = pc::RequestView::parse(&input)?;
    let (r, adapter) = crate::cmd::distill::resolve_source(id, harness)?;
    let settings = Settings::default();
    let root = std::env::temp_dir();
    // With --cut-tokens, the cut is the newest message at which the fill stood at or under it.
    let mut snapshot: Option<(pc::Cut, usize, usize)> = None;
    // Each message's id, kind and opening text, so a miss can name the message it stopped at.
    let mut seen: Vec<(Option<String>, MessageKind, String)> = Vec::new();
    let live = read_live(&r, adapter.as_ref(), upto_bytes, &settings, &root, |live, m| {
        let mut head = m.text().unwrap_or_default();
        head.truncate(head.floor_char_boundary(160));
        seen.push((m.id.clone(), m.kind, head));
        if let (Some(limit), Some(fill), Some(cut)) = (cut_tokens, live.window.fill(), live.window.cut()) {
            if fill <= limit {
                snapshot = Some((cut, live.window.anchors.len(), live.window.boundaries.len()));
            }
        }
    })?;
    let (cut, anchors) = match (cut_tokens, snapshot) {
        (None, _) => live.window.cut_now().context("the session's window is empty")?,
        (Some(_), Some((cut, n, boundaries))) if boundaries == live.window.boundaries.len() => {
            (cut, live.window.anchors[..n].iter().map(|p| p.anchor.clone()).collect())
        }
        (Some(t), _) => anyhow::bail!("the window never stood at or under {t} tokens"),
    };
    let record = Record {
        v: pc::RECORD_VERSION,
        harness: r.harness.as_str().into(),
        session: r.id.clone(),
        key: None,
        window: pc::WindowRef {
            boundaries: live.window.boundaries.len(),
            start: live.window.start,
        },
        cut: cut.clone(),
        anchors,
        profile: view
            .instruction
            .as_ref()
            .map_or_else(String::new, |i| pc::profile_id(&i.text)),
        model: String::new(),
        created: chrono::Utc::now(),
        base: None,
        compact_at: None,
        bounds: Vec::new(),
        unresolved_cites: 0,
        narrative: String::new(),
        carry: Default::default(),
        text: String::new(),
    };
    let verdict = pc::validate(&view, &record);
    let mut out = json!({
        "session": r.id,
        "request_session": view.session.as_ref().map(|s| &s.id),
        "request_session_source": view.session.as_ref().map(|s| &s.source),
        "session_matches": view.session.as_ref().is_some_and(|s| s.id == r.id),
        "request_messages": view.messages.len(),
        "instruction_profile": record.profile,
        "window_messages": live.window.messages - live.window.start,
        "anchors": record.anchors.len(),
        "cut_index": cut.index,
        "cut_tokens": cut.tokens,
        "cut_clean": cut.clean,
        "fill": live.window.fill(),
    });
    match verdict {
        Ok(m) => {
            let tail = view.tail(m.tail_from);
            out["verdict"] = json!("match");
            out["tail_from"] = json!(m.tail_from);
            out["tail_messages"] = json!(tail.len());
            out["tail_text_bytes"] =
                json!(tail
                    .iter()
                    .map(|t| t.texts.iter().map(String::len).sum::<usize>()
                        + t.notes.iter().map(String::len).sum::<usize>())
                    .sum::<usize>());
        }
        Err(miss) => {
            out["verdict"] = json!("miss");
            out["reason"] = json!(miss.to_string());
            if let pc::Miss::Anchor { index, .. } = miss {
                if let Some((id, kind, head)) = live.window.anchors.get(index).and_then(|p| seen.get(p.index)) {
                    out["miss_message"] = json!({"id": id, "kind": kind, "text": head});
                }
            }
        }
    }
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

// ───────────────────────────── commit / observe ─────────────────────────────

fn miss(root: &Path, session: Option<&str>, class: &str, reason: &str, started: Instant, check: bool) -> Result<()> {
    let event = if check {
        json!({"event": "precheck", "outcome": "pass_through", "session": session, "class": class,
               "secs": started.elapsed().as_secs_f64()})
    } else {
        json!({"event": "commit", "outcome": "miss", "session": session, "class": class,
               "reason": reason, "secs": started.elapsed().as_secs_f64()})
    };
    let _ = store::event(root, event);
    eprintln!("precompact commit: miss ({class}): {reason}");
    std::process::exit(MISS_EXIT);
}

/// The commit: inside the harness's compaction, check the session's ready record against the
/// conversation the harness is about to compact, and hand back the summary and where the kept
/// messages start. Every miss exits 3; the harness then compacts as its own. With `check`, the
/// same check serves nothing and counts no miss: the plugin asks it before the harness starts its
/// own background summary, which a pass vetoes.
fn commit(serve: Option<String>, check: bool, harness: Option<String>) -> Result<()> {
    let started = Instant::now();
    let root = store::root()?;
    let settings = cv_core::config::load().precompact.unwrap_or_default();
    let serve = match serve {
        Some(s) => s.parse::<pc::Serve>().or_else(usage)?,
        None => settings.serve,
    };
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).context("reading the compaction on stdin")?;
    let input: Value = serde_json::from_str(&text).context("the compaction on stdin is not JSON")?;
    drop(text);
    let view = pc::RequestView::parse(&input)?;
    drop(input);
    let Some(session) = view.session.as_ref().map(|s| s.id.clone()) else {
        return miss(
            &root,
            None,
            "no_session",
            "the compaction names no session",
            started,
            check,
        );
    };
    if serve == pc::Serve::Native {
        return miss(
            &root,
            Some(&session),
            "serve_native",
            "this seat serves the harness's own compaction",
            started,
            check,
        );
    }
    let harness = harness
        .or_else(|| view.harness.clone())
        .unwrap_or_else(|| "claude".into());
    let Some(record) = store::load_record(&root, &harness, &session)? else {
        return miss(
            &root,
            Some(&session),
            "no_record",
            "no ready summary for this session",
            started,
            check,
        );
    };
    let budget = record
        .compact_at
        .or(settings.compact_at)
        .map(|c| (c as f64 * settings.fit_fraction) as u64);
    let c = match pc::commit(&view, &record, budget) {
        Ok(c) => c,
        Err(why) => return miss(&root, Some(&session), why.class(), &why.to_string(), started, check),
    };
    if check {
        println!("{}", json!({"outcome": "would_serve", "tokens": c.tokens}));
        let _ = store::event(
            &root,
            json!({"event": "precheck", "outcome": "veto", "harness": harness, "session": session,
                   "tokens": c.tokens, "secs": started.elapsed().as_secs_f64()}),
        );
        return Ok(());
    }
    let anchor = record.key.as_ref().map(|k| k.anchor.as_str()).unwrap_or("");
    let summary = format!(
        "{} (session {session}, summarized through message {anchor}). Any [m:ID] below opens in \
         full with `cv show {session} --around ID`.\n\n{}",
        pc::SERVED_PREAMBLE,
        record.text
    );
    let out = json!({"outcome": "served", "summary": summary, "keep_from": c.keep_from,
                     "anchors": c.anchors, "tokens": c.tokens, "anchor": anchor,
                     "created": record.created, "model": record.model});
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &out)?;
    stdout.flush()?;
    let _ = store::event(
        &root,
        json!({"event": "commit", "outcome": "served", "harness": harness, "session": session,
               "anchor": anchor, "anchors": c.anchors, "keep_from": c.keep_from,
               "messages": view.messages.len(), "tokens": c.tokens,
               "record_cut_tokens": record.cut.tokens, "summary_bytes": summary.len(),
               "record_age_secs": (chrono::Utc::now() - record.created).num_seconds(),
               "secs": started.elapsed().as_secs_f64()}),
    );
    eprintln!(
        "precompact commit: served {} ({} commitments, {} of {} message(s) kept) in {:.2}s",
        short_id(&session),
        c.anchors,
        view.messages.len().saturating_sub(c.keep_from),
        view.messages.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// One compaction as a harness plugin saw it, appended as a `compacted` event: which method
/// served (`cv` or `native`), its wall time, the tokens before and after, what was kept and
/// carried, and the commit's miss when cv was asked and missed.
fn observe() -> Result<()> {
    let root = store::root()?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).context("reading the observation on stdin")?;
    let Value::Object(mut row) = serde_json::from_str::<Value>(&text).context("the observation is not JSON")? else {
        return usage("the observation on stdin is not a JSON object");
    };
    for key in ["session", "method"] {
        if !row.get(key).is_some_and(Value::is_string) {
            return usage(format!("the observation has no `{key}`"));
        }
    }
    row.insert("event".into(), "compacted".into());
    store::event(&root, Value::Object(row))
}

// ───────────────────────────── profiles, ls, report ─────────────────────────────

fn profile(action: ProfileCmd) -> Result<()> {
    let root = store::root()?;
    match action {
        ProfileCmd::Import { request, harness } => {
            let view = pc::RequestView::parse(&read_request(&request)?)?;
            let ins = view
                .instruction
                .context("the request has no user text to take as the compaction instruction")?;
            let profile = pc::Profile {
                id: pc::profile_id(&ins.text),
                harness,
                client: view.client,
                captured: chrono::Utc::now(),
                text: ins.text,
            };
            let new = store::save_profile(&root, &profile)?;
            println!(
                "{} {} profile {} ({} bytes)",
                if new { "captured" } else { "already had" },
                profile.harness,
                profile.id,
                profile.text.len()
            );
        }
        ProfileCmd::Ls => {
            for p in store::profiles(&root)? {
                let head: String = p.text.chars().take(72).collect::<String>().replace('\n', " ");
                println!(
                    "{}  {:<8} {}  {}  {head}…",
                    p.id,
                    p.harness,
                    p.captured.format("%Y-%m-%d %H:%M"),
                    p.client.as_deref().unwrap_or("-")
                );
            }
        }
    }
    Ok(())
}

fn ls(json: bool) -> Result<()> {
    let root = store::root()?;
    let records = store::records(&root, None)?;
    if json {
        let rows: Vec<Value> = records
            .iter()
            .map(|(path, r)| {
                json!({"harness": r.harness, "session": r.session, "cut": r.cut, "window": r.window,
                       "key": r.key, "ready": r.key.as_ref().is_some_and(pc::Key::admits_now),
                       "anchors": r.anchors.len(), "profile": r.profile, "model": r.model,
                       "created": r.created, "text_bytes": r.text.len(), "path": path})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if records.is_empty() {
        println!("no precomputed summaries in {}", root.display());
    }
    for (_, r) in records {
        println!(
            "{:<8} {}  msg {} · {} tokens · {} anchors · {} bytes · {} · {}",
            r.harness,
            short_id(&r.session),
            r.cut.index,
            r.cut.tokens.map_or("?".into(), fmt_int),
            r.anchors.len(),
            fmt_int(r.text.len() as u64),
            r.model,
            r.created.format("%Y-%m-%d %H:%M"),
        );
    }
    Ok(())
}

fn median(v: &mut [u64]) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[v.len() / 2])
}

/// The comparison the serve setting is chosen by: per method (cv's, the harness's precompute, the
/// harness's own), the stall and the tokens before and after, from the transcript's own
/// compaction records (the follower's `compaction` events, attributed to cv when a commit served
/// the session since its last compaction) and from the harness plugin's `compacted` events.
/// A stall as the report shows it: a served one is milliseconds, and would round to `0s`.
fn fmt_stall(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        crate::util::fmt_duration(ms)
    }
}

fn report(json_out: bool) -> Result<()> {
    let root = store::root()?;
    let text = match std::fs::read_to_string(root.join("events.jsonl")) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    const METHODS: [&str; 3] = ["cv", "native precompute", "native"];
    #[derive(Default)]
    struct Col {
        ms: Vec<u64>,
        before: Vec<u64>,
        after: Vec<u64>,
        carried: Vec<u64>,
    }
    let take = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64);
    // Per session, the newest commit outcome since its last compaction decides who served it.
    let mut last_commit: HashMap<String, String> = HashMap::new();
    let mut transcript: HashMap<&'static str, Col> = HashMap::new();
    let mut plugin: HashMap<&'static str, Col> = HashMap::new();
    let (mut served, mut missed, mut written) = (0u64, 0u64, 0u64);
    let mut classes: HashMap<String, u64> = HashMap::new();
    let (mut vetoed, mut passed_through) = (0u64, 0u64);
    // Per summary model: summaries written, input tokens, output tokens.
    let mut models: std::collections::BTreeMap<String, (u64, u64, u64)> = Default::default();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let session = v.get("session").and_then(Value::as_str).unwrap_or("").to_owned();
        match v.get("event").and_then(Value::as_str) {
            Some("commit") => {
                let outcome = v.get("outcome").and_then(Value::as_str).unwrap_or("").to_owned();
                if outcome == "served" {
                    served += 1;
                } else {
                    missed += 1;
                    let class = v.get("class").and_then(Value::as_str).unwrap_or("other");
                    *classes.entry(class.to_owned()).or_default() += 1;
                }
                last_commit.insert(session, outcome);
            }
            Some("written") => {
                written += 1;
                let row = models
                    .entry(v.get("model").and_then(Value::as_str).unwrap_or("").to_owned())
                    .or_default();
                row.0 += 1;
                row.1 += take(&v, "input_tokens").unwrap_or(0);
                row.2 += take(&v, "output_tokens").unwrap_or(0);
            }
            Some("precheck") => match v.get("outcome").and_then(Value::as_str) {
                Some("veto") => vetoed += 1,
                _ => passed_through += 1,
            },
            Some("compaction") => {
                let method = if last_commit.get(&session).map(String::as_str) == Some("served") {
                    METHODS[0]
                } else if v.get("native").and_then(Value::as_bool) == Some(true) {
                    METHODS[1]
                } else {
                    METHODS[2]
                };
                last_commit.remove(&session);
                let col = transcript.entry(method).or_default();
                col.ms.extend(take(&v, "duration_ms"));
                col.before.extend(take(&v, "pre_tokens"));
                col.after.extend(take(&v, "post_tokens"));
            }
            Some("compacted") => {
                let method = match v.get("method").and_then(Value::as_str) {
                    Some("cv") => METHODS[0],
                    _ if v.get("precomputed").and_then(Value::as_bool) == Some(true) => METHODS[1],
                    _ => METHODS[2],
                };
                let col = plugin.entry(method).or_default();
                col.ms.extend(take(&v, "ms"));
                col.before.extend(take(&v, "tokens_before"));
                col.after.extend(take(&v, "tokens_after"));
                col.carried.extend(take(&v, "summary_bytes"));
            }
            _ => {}
        }
    }
    // The harness records no tokens for a compaction a hook answered, so a served row reads them
    // from the transcript's records of the same compactions.
    if let (Some(col), Some(seen)) = (plugin.get_mut(METHODS[0]), transcript.get(METHODS[0])) {
        if col.before.is_empty() {
            col.before.clone_from(&seen.before);
        }
        if col.after.is_empty() {
            col.after.clone_from(&seen.after);
        }
    }
    let rows = |source: &str, cols: &mut HashMap<&'static str, Col>| -> Vec<Value> {
        METHODS
            .iter()
            .map(|m| {
                let mut c = cols.remove(m).unwrap_or_default();
                json!({"source": source, "method": m, "compactions": c.ms.len().max(c.after.len()),
                       "median_ms": median(&mut c.ms), "max_ms": c.ms.iter().max(),
                       "median_tokens_before": median(&mut c.before),
                       "median_tokens_after": median(&mut c.after),
                       "median_summary_bytes": median(&mut c.carried)})
            })
            .collect()
    };
    let mut stalls = rows("transcript", &mut transcript);
    stalls.extend(rows("plugin", &mut plugin));
    let model_rows: Vec<Value> = models
        .iter()
        .map(|(m, (n, i, o))| json!({"model": m, "summaries": n, "input_tokens": i, "output_tokens": o}))
        .collect();
    let out = json!({"summaries_written": written, "commits_served": served, "commits_missed": missed,
                     "miss_classes": classes, "precompute_vetoed": vetoed,
                     "precompute_passed_through": passed_through, "summary_models": model_rows,
                     "methods": stalls});
    if json_out {
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    println!("summaries written {written} · commits served {served} · missed {missed}");
    println!("native precompute vetoed {vetoed} · passed through {passed_through}");
    for (m, (n, i, o)) in &models {
        println!(
            "  summary model {m}: {n} written · {} in · {} out",
            fmt_int(*i),
            fmt_int(*o)
        );
    }
    for (class, n) in &classes {
        println!("  miss {n}× {class}");
    }
    let num = |v: &Value| v.as_u64().map_or("-".into(), fmt_int);
    for row in stalls {
        println!(
            "{:<10} {:<18} {:>4} compaction(s) · stall median {} · max {} · tokens {} -> {}",
            row["source"].as_str().unwrap_or(""),
            row["method"].as_str().unwrap_or(""),
            row["compactions"],
            row["median_ms"].as_u64().map_or("-".into(), fmt_stall),
            row["max_ms"].as_u64().map_or("-".into(), fmt_stall),
            num(&row["median_tokens_before"]),
            num(&row["median_tokens_after"]),
        );
    }
    Ok(())
}

// ───────────────────────────── swap: an explicit new session ─────────────────────────────

fn swap(id: &str, harness: Option<Harness>, dry_run: bool) -> Result<()> {
    let (r, adapter) = crate::cmd::distill::resolve_source(id, harness)?;
    let root = store::root()?;
    let Some(record) = store::load_record(&root, r.harness.as_str(), &r.id)? else {
        return usage(format!("no precomputed summary for {}", short_id(&r.id)));
    };
    // The transcript's own check: the record's anchors are still this window's. The source's
    // identity before it is the generation the copy must still describe when it is written.
    let source = adapter.resume_source(&r);
    let generation = cv_core::source::FileId::at(&source);
    let started = Instant::now();
    let live = read_live(&r, adapter.as_ref(), None, &Settings::default(), &root, |_, _| {})?;
    let holds = record.window.boundaries == live.window.boundaries.len()
        && record.anchors.len() <= live.window.anchors.len()
        && record
            .anchors
            .iter()
            .zip(&live.window.anchors)
            .all(|(a, p)| *a == p.anchor);
    if !holds {
        return usage(format!(
            "{}'s summary no longer matches its transcript (it compacted or was rewound since)",
            short_id(&r.id)
        ));
    }
    let cut_id = record
        .cut
        .id
        .as_deref()
        .context("the record's cut has no message id to splice after")?;
    let Some(copy) = adapter.compacted_copy(&r, cut_id, &record.text, record.cut.tokens) else {
        return usage(format!("{} has no compacted-copy writer yet", r.harness));
    };
    let copy = copy?;
    same_generation(&source, generation)?;
    eprintln!(
        "precompact swap: {} -> {} (summary to msg {}, {} record(s) after the cut, {} bytes) in {:.2}s",
        short_id(&r.id),
        short_id(&copy.id),
        record.cut.index,
        copy.carried,
        fmt_int(copy.bytes.len() as u64),
        started.elapsed().as_secs_f64()
    );
    if dry_run {
        return Ok(());
    }
    anyhow::ensure!(!cv_core::env::frozen(), "--frozen forbids writing a session");
    cv_core::publish::publish_reader(
        &copy.path,
        "precompact-swap",
        copy.bytes.as_slice(),
        cv_core::publish::Claim::New,
    )?;
    // A swap is a new run: its lineage is recorded, or it is lost.
    let _ = store::event(
        &root,
        json!({"event": "swap", "harness": r.harness.as_str(), "parent": r.id, "session": copy.id,
               "anchor": cut_id, "carried": copy.carried}),
    );
    println!("{}", copy.path.display());
    for line in crate::cmd::port::resume_lines(r.harness, &copy.id, r.cwd.as_deref()) {
        eprintln!("  {line}");
    }
    Ok(())
}

/// The swap's commit check: `path` is still the file the anchor check read (`before`), unchanged,
/// and ends on a whole record. A write since (the session went on) or a torn last line (a writer
/// mid-record) leaves the copy short of the session, so the swap refuses it.
fn same_generation(path: &Path, before: Option<cv_core::source::FileId>) -> Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let now = cv_core::source::FileId::at(path);
    anyhow::ensure!(
        now.is_some() && now == before,
        "{} was written to during the swap; swap it when it is idle",
        path.display()
    );
    if now.is_some_and(|id| id.len > 0) {
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0u8];
        file.read_exact(&mut last)?;
        anyhow::ensure!(
            last[0] == b'\n',
            "{} ends inside a record (a writer mid-line); swap it when it is idle",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed summary's pause doubles from [`RETRY_FIRST`] and stops at [`RETRY_MAX`].
    #[test]
    fn a_failed_summary_pause_doubles_to_its_ceiling() {
        assert_eq!(summary_pause(1), RETRY_FIRST);
        assert_eq!(summary_pause(2), RETRY_FIRST * 2);
        assert_eq!(summary_pause(4), RETRY_FIRST * 8);
        assert_eq!(summary_pause(8), RETRY_MAX);
        assert_eq!(summary_pause(u32::MAX), RETRY_MAX);
        assert_eq!(summary_pause(0), RETRY_FIRST, "no failure yet reads as the first pause, never a zero one");
    }

    /// A swap commits only the generation its anchor check read: a write since, a torn last
    /// line or a missing file refuses it.
    #[test]
    fn a_swap_commits_only_the_generation_it_checked() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("cv-swap-generation-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "{\"a\":1}\n").unwrap();
        let before = cv_core::source::FileId::at(&path);
        assert!(same_generation(&path, before).is_ok());
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"a\":2}\n").unwrap();
        assert!(same_generation(&path, before).is_err(), "written to since the check");
        file.write_all(b"{\"a\":").unwrap();
        let torn = cv_core::source::FileId::at(&path);
        assert!(same_generation(&path, torn).is_err(), "a torn last line");
        assert!(same_generation(&dir.join("gone.jsonl"), None).is_err(), "no file");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A pointer is what the agent did with a rule: named it in a reply or a tool call, or drew a
    /// refusal naming it. A rule a hook only injected, or ordinary tool output naming one, makes
    /// none.
    #[test]
    fn pointers_come_from_what_the_agent_did_with_a_rule() {
        let message = |role, kind, id: &str, block| {
            let mut m = Message::new(role);
            m.kind = kind;
            m.id = Some(id.into());
            m.content.push(block);
            m
        };
        let text = |t: &str| Block::Text {
            text: t.to_string().into(),
        };
        let result = |t: &str, is_error| Block::ToolResult {
            tool_use_id: "t1".into(),
            content: t.to_string().into(),
            is_error,
            tool_name: None,
            status: None,
            details: None,
        };
        let settings = Settings {
            rule_patterns: vec![r"store `([a-z-]+)`".into(), r"store get ([a-z-]+)".into()],
            ..Default::default()
        };
        let mut live = Live::new(SessionRef::new("s", Harness::Claude, Path::new("/s.jsonl")));
        for m in [
            message(
                Role::User,
                MessageKind::InjectedContext,
                "i1",
                text("PREMISE: see store `injected-rule`"),
            ),
            message(
                Role::Assistant,
                MessageKind::Reply,
                "a1",
                text("per store `named-rule` I stop here"),
            ),
            message(
                Role::Assistant,
                MessageKind::Reply,
                "a2",
                Block::ToolUse {
                    id: "t1".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "arc store get fetched-rule"}).into(),
                    namespace: None,
                },
            ),
            message(
                Role::Tool,
                MessageKind::ToolResult,
                "r1",
                result("refused: store `refusal-rule`", true),
            ),
            message(
                Role::Tool,
                MessageKind::ToolResult,
                "r2",
                result("listing: store `output-rule`", false),
            ),
        ] {
            live.observe(&m, &settings, Path::new("/nonexistent"), true).unwrap();
        }
        let found: Vec<(&str, &str)> = live.found.iter().map(|(_, m, id)| (m.as_str(), id.as_str())).collect();
        assert_eq!(
            found,
            [("a1", "named-rule"), ("a2", "fetched-rule"), ("r1", "refusal-rule")]
        );
        let ids: Vec<&str> = live.ids.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(ids, ["i1", "a1", "a2", "r1", "r2"], "every message id is citable");
    }

    /// A read that fails after delivering messages is delivered again from the last accepted
    /// position; what was observed of it is skipped, and only the new messages are taken.
    #[test]
    fn a_failed_read_is_taken_once() {
        let r = SessionRef::new("s", Harness::Claude, Path::new("/s.jsonl"));
        let mut live = Live::new(r);
        let took = |live: &mut Live, n: usize| (0..n).filter(|_| live.take()).count();
        assert_eq!(took(&mut live, 5), 5);
        live.settle(true);
        assert_eq!(took(&mut live, 3), 3);
        live.settle(false);
        // Delivered again: the 3 already taken, then 2 new; then it fails once more.
        assert_eq!(took(&mut live, 4), 1);
        live.settle(false);
        assert_eq!(took(&mut live, 6), 2);
        live.settle(true);
        assert_eq!((live.delivered, live.accepted, live.redelivered), (11, 11, 0));
        // Let go and back: read from the start, so all 11 come again before 1 new one.
        live.restart();
        assert_eq!(took(&mut live, 12), 1);
        live.settle(true);
        assert_eq!((live.delivered, live.accepted, live.redelivered), (12, 12, 0));
        // A first read that fails is read from the start again, skipped as far as it got.
        let mut live = Live::new(SessionRef::new("t", Harness::Claude, Path::new("/t.jsonl")));
        assert_eq!(took(&mut live, 4), 4);
        live.settle(false);
        live.restart();
        assert_eq!(took(&mut live, 6), 2);
    }
}
