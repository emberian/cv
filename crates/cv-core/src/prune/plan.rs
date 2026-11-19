//! `cv prune --plan`: a caller's judgment of each payload, in place of the size/age heuristics.
//!
//! A plan names tool results by `tool_use_id` and records by `uuid`, and says of each one
//! `keep`, `pointer`, `precis`, `excerpt` or `drop`; it can also drop attachment records by
//! kind. It is one more source of decisions for the snip path: a planned payload is stashed in
//! the sidecar like any snip, its replacement being the usual marker followed by the précis and
//! any kept passages or lines. A dropped record or call is stashed too, and taken out. Every
//! unlisted record stays verbatim. [`PlanCheck`] validates the plan against the session during
//! the decision pass, before anything is staged, and notes the one thing a selection can get
//! wrong without breaking the session: a reply kept with none of what it read
//! ([`Unsupported`]).
use super::{classify_source, MARKER_PREFIX};
use crate::harness::claude::record::Facts;
use crate::harness::store::document::Fields;
use crate::json::JsonView;
use anyhow::Result;
use std::collections::{HashMap, HashSet};

/// The plan file (`cv prune --plan <plan.json>`).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// Planned tool results, by `tool_use_id`.
    #[serde(default)]
    pub results: HashMap<String, Planned>,
    /// Planned records, by `uuid`: a person's message, a reply, an attachment.
    #[serde(default)]
    pub messages: HashMap<String, Planned>,
    #[serde(default)]
    pub attachments: PlanAttachments,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanAttachments {
    /// `attachment.type` values whose records are removed (stashed by record uuid).
    #[serde(default)]
    pub drop_kinds: HashSet<String>,
}

/// One planned tool result or record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
pub enum Planned {
    /// Verbatim (the default for everything unlisted too).
    Keep,
    /// The marker alone: `cv cat` reopens the original.
    Pointer,
    /// Marker, then the précis.
    Precis { precis: String },
    /// Marker, the précis (if any), then what is kept verbatim: each passage in `keep` (a
    /// literal substring of the payload) and each range in `lines`, in payload order.
    Excerpt {
        #[serde(default)]
        keep: Vec<String>,
        #[serde(default)]
        lines: Vec<Lines>,
        #[serde(default)]
        precis: String,
    },
    /// Taken out of the session and stashed: a record whole, a tool result with its call.
    Drop,
}

impl Planned {
    fn name(&self) -> &'static str {
        match self {
            Planned::Keep => "keep",
            Planned::Pointer => "pointer",
            Planned::Precis { .. } => "precis",
            Planned::Excerpt { .. } => "excerpt",
            Planned::Drop => "drop",
        }
    }
    /// The payload is replaced by a marker (with or without a précis or kept parts).
    pub(super) fn replaces(&self) -> bool {
        matches!(
            self,
            Planned::Pointer | Planned::Precis { .. } | Planned::Excerpt { .. }
        )
    }
}

/// Lines of a payload, 1-based and inclusive: `"7"`, `"3-10"`, or `"40-"` through the last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "serde_json::Value")]
pub struct Lines {
    pub first: usize,
    /// `None`: through the last line.
    pub last: Option<usize>,
}

impl TryFrom<serde_json::Value> for Lines {
    type Error = String;
    fn try_from(value: serde_json::Value) -> std::result::Result<Self, String> {
        let bad = || format!("{value}: lines are 1-based: \"7\", \"3-10\" or \"40-\"");
        let number = |s: &str| s.trim().parse::<usize>().ok().filter(|n| *n >= 1);
        let lines = match &value {
            serde_json::Value::Number(n) => n
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| *n >= 1)
                .map(|n| Lines {
                    first: n,
                    last: Some(n),
                }),
            serde_json::Value::String(s) => match s.split_once('-') {
                None => number(s).map(|n| Lines {
                    first: n,
                    last: Some(n),
                }),
                Some((first, last)) if last.trim().is_empty() => number(first).map(|first| Lines { first, last: None }),
                Some((first, last)) => number(first)
                    .zip(number(last))
                    .filter(|(first, last)| first <= last)
                    .map(|(first, last)| Lines {
                        first,
                        last: Some(last),
                    }),
            },
            _ => None,
        };
        lines.ok_or_else(bad)
    }
}

impl Plan {
    pub fn parse(text: &str) -> Result<Plan> {
        Ok(serde_json::from_str(text)?)
    }
    pub(super) fn get(&self, id: Option<&str>) -> Option<&Planned> {
        id.and_then(|id| self.results.get(id))
    }
    pub(super) fn message(&self, uuid: Option<&str>) -> Option<&Planned> {
        uuid.and_then(|uuid| self.messages.get(uuid))
    }
}

/// A plan the session refuses: unknown ids, passages or lines that are not in their payload,
/// excerpts of image payloads, an action a record cannot take. Nothing was staged or written.
#[derive(Debug)]
pub struct PlanRefused(pub Vec<String>);
impl std::fmt::Display for PlanRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the prune plan does not fit this session:")?;
        for problem in &self.0 {
            write!(f, "\n  {problem}")?;
        }
        Ok(())
    }
}
impl std::error::Error for PlanRefused {}

/// A kept reply whose turn's tool results the plan all dropped or left as bare pointers: the
/// reply states conclusions and nothing it read to reach them is still in the session. A
/// warning for the caller, never a refusal: the result is a valid session either way.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Unsupported {
    /// The reply record's `uuid` (the turn's last kept reply).
    pub reply: String,
    /// How many tool results its turn held, none of them kept.
    pub results: usize,
}

/// What the plan changed, for the `--json` report's `plan` object.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PlanReport {
    pub keep: usize,
    pub pointer: usize,
    pub precis: usize,
    pub excerpt: usize,
    /// Tool results taken out with their calls.
    pub results_dropped: usize,
    /// Records taken out by `uuid` (a record a dropped call or result left empty included).
    pub records_dropped: usize,
    pub attachments_dropped: usize,
    /// Payload bytes the planned replacements and dropped records removed.
    pub bytes_saved: u64,
    /// Kept replies that rest on nothing kept ([`Unsupported`]).
    pub rests_on_nothing: Vec<Unsupported>,
}

/// What a planned result does to its content, decided the same way by the check and the snip.
pub(super) enum Effect {
    /// Left verbatim: no content, already a prune marker, or nothing classifiable.
    Verbatim(Option<String>),
    Replace,
}

/// Whether a planned replacement applies to `content` (with the reason when it does not).
pub(super) fn effect(id: &str, content: Option<JsonView<'_>>) -> Result<Effect> {
    let content = match content {
        Some(content) if !content.is_null()? => content,
        _ => {
            return Ok(Effect::Verbatim(Some(format!(
                "{id}: no content to replace; kept verbatim"
            ))))
        }
    };
    if content.is_string()? {
        if is_marker(content)? {
            return Ok(Effect::Verbatim(Some(format!(
                "{id}: already a prune marker; kept verbatim"
            ))));
        }
    } else if classify_source(content)?.0 == "none" {
        return Ok(Effect::Verbatim(Some(format!(
            "{id}: content is neither text nor blocks; kept verbatim"
        ))));
    }
    Ok(Effect::Replace)
}

/// A string that already is a prune marker.
pub(super) fn is_marker(text: JsonView<'_>) -> Result<bool> {
    let mut prefix = String::new();
    text.for_each_str_chunk(|chunk| {
        if prefix.len() < MARKER_PREFIX.len() {
            prefix.extend(chunk.chars().take(MARKER_PREFIX.len()));
        }
        Ok(())
    })?;
    Ok(prefix.starts_with(MARKER_PREFIX))
}

/// A string view, owned.
pub(super) fn string(view: JsonView<'_>) -> Result<String> {
    let mut text = String::new();
    view.for_each_str_chunk(|chunk| {
        text.push_str(chunk);
        Ok(())
    })?;
    Ok(text)
}

/// The payload's text, as the model read it: the string, or its text blocks joined by newlines.
/// `Err(kind)` for an image or mixed payload.
pub(super) fn payload_text(content: JsonView<'_>) -> Result<std::result::Result<String, String>> {
    if content.is_string()? {
        return Ok(Ok(string(content)?));
    }
    let kind = classify_source(content)?.0;
    if kind != "text" {
        return Ok(Err(kind));
    }
    let mut text = String::new();
    let mut first = true;
    for item in content.array()?.into_iter().flatten() {
        let Some(item) = Fields::parse(item?, &["type", "text"])? else {
            continue;
        };
        if item.string("type")?.as_deref() != Some("text") {
            continue;
        }
        if let Some(value) = item.get("text").filter(|v| v.is_string().unwrap_or(false)) {
            if !first {
                text.push('\n');
            }
            first = false;
            value.for_each_str_chunk(|chunk| {
                text.push_str(chunk);
                Ok(())
            })?;
        }
    }
    Ok(Ok(text))
}

/// The byte range of `lines` in `text` (a trailing newline ends the last line, it does not
/// start another), or why the text has no such lines.
fn line_span(text: &str, lines: Lines) -> std::result::Result<(usize, usize), String> {
    let mut starts = vec![0];
    starts.extend(memchr::memchr_iter(b'\n', text.as_bytes()).map(|at| at + 1));
    if starts.last() == Some(&text.len()) && starts.len() > 1 {
        starts.pop();
    }
    let count = starts.len();
    if lines.first > count {
        return Err(format!("line {} is past the payload's {count} line(s)", lines.first));
    }
    let last = lines.last.unwrap_or(count).min(count);
    let whole = text.strip_suffix('\n').map_or(text.len(), str::len);
    let end = starts.get(last).map_or(whole, |next| next - 1);
    Ok((starts[lines.first - 1], end.max(starts[lines.first - 1])))
}

/// What a plan's excerpt keeps of `text` that the text does not hold, one problem each.
fn excerpt_problems(id: &str, planned: &Planned, text: &str) -> Vec<String> {
    let Planned::Excerpt { keep, lines, .. } = planned else {
        return Vec::new();
    };
    let mut problems = Vec::new();
    if keep.is_empty() && lines.is_empty() {
        problems.push(format!(
            "{id}: an excerpt names what it keeps: passages (`keep`) or `lines`"
        ));
    }
    for passage in keep.iter().filter(|p| !text.contains(p.as_str())) {
        let head: String = passage.chars().take(80).collect();
        let more = if passage.chars().count() > 80 { "…" } else { "" };
        problems.push(format!("{id}: kept passage is not in the payload: {head:?}{more}"));
    }
    for range in lines {
        if let Err(why) = line_span(text, *range) {
            problems.push(format!("{id}: {why}"));
        }
    }
    problems
}

/// The replacement content: the marker, a newline, the précis, then for an excerpt each kept
/// passage and line range in the order it appears in `text`, a passage after a `[…]` line and
/// a line range after one naming its lines.
pub(super) fn replacement(marker: &str, planned: &Planned, text: Option<&str>) -> String {
    let mut out = String::from(marker);
    match planned {
        Planned::Keep | Planned::Pointer | Planned::Drop => {}
        Planned::Precis { precis } => {
            out.push('\n');
            out.push_str(precis);
        }
        Planned::Excerpt { keep, lines, precis } => {
            if !precis.is_empty() {
                out.push('\n');
                out.push_str(precis);
            }
            let text = text.unwrap_or_default();
            // (where it starts, the line before it, what is kept)
            let mut kept: Vec<(usize, String, &str)> = keep
                .iter()
                .map(|p| {
                    (
                        text.find(p.as_str()).unwrap_or(usize::MAX),
                        "[…]".to_owned(),
                        p.as_str(),
                    )
                })
                .collect();
            for range in lines {
                if let Ok((start, end)) = line_span(text, *range) {
                    let label = match range.last {
                        Some(last) if last == range.first => format!("[… line {}]", range.first),
                        Some(last) => format!("[… lines {}-{last}]", range.first),
                        None => format!("[… lines {}-]", range.first),
                    };
                    kept.push((start, label, &text[start..end]));
                }
            }
            kept.sort_by_key(|(at, _, _)| *at);
            for (_, label, part) in kept {
                out.push('\n');
                out.push_str(&label);
                out.push('\n');
                out.push_str(part);
            }
        }
    }
    out
}

/// What the write pass needs of a checked plan.
#[derive(Default)]
pub(super) struct Checked {
    pub warnings: Vec<String>,
    /// Tool calls taken out with their results: planned `drop`, or held by a dropped record.
    pub drop_tools: HashSet<String>,
    pub rests_on_nothing: Vec<Unsupported>,
}

/// One turn's facts for [`Unsupported`]: the tool results it held and its replies, in order.
#[derive(Default)]
struct Turn {
    results: Vec<String>,
    /// (uuid, came after a tool result)
    replies: Vec<(String, bool)>,
}

/// The plan checked against every record during the decision pass.
#[derive(Default)]
pub(super) struct PlanCheck {
    seen: HashSet<String>,
    seen_messages: HashSet<String>,
    problems: Vec<String>,
    pub warnings: Vec<String>,
    drop_tools: HashSet<String>,
    /// Tool results and calls by where the plan found them, to name a contradiction.
    planned_results: HashSet<String>,
    turns: Vec<Turn>,
}

impl PlanCheck {
    /// One `tool_result` block of a user record. Returns `(planned replacement applies, planned
    /// keep)`.
    pub(super) fn result(&mut self, plan: &Plan, id: &str, content: Option<JsonView<'_>>) -> Result<(bool, bool)> {
        let Some(planned) = plan.results.get(id) else {
            return Ok((false, false));
        };
        self.seen.insert(id.to_owned());
        match planned {
            Planned::Keep => {
                self.planned_results.insert(id.to_owned());
                return Ok((false, true));
            }
            Planned::Drop => {
                self.drop_tools.insert(id.to_owned());
                return Ok((false, false));
            }
            _ => {}
        }
        self.planned_results.insert(id.to_owned());
        if let Effect::Verbatim(why) = effect(id, content)? {
            self.warnings.extend(why);
            return Ok((false, false));
        }
        if let (Planned::Excerpt { .. }, Some(content)) = (planned, content) {
            match payload_text(content)? {
                Err(kind) => self
                    .problems
                    .push(format!("{id}: an excerpt needs a text payload; this one is {kind}")),
                Ok(text) => self.problems.extend(excerpt_problems(id, planned, &text)),
            }
        }
        Ok((true, false))
    }

    /// One record the plan may name by `uuid`. `text` is the record's text payloads (a string
    /// `message.content`, each text block, each rendered item of an attachment). Returns
    /// `(the record is dropped, its text is replaced)`.
    pub(super) fn message(&mut self, plan: &Plan, facts: &Facts, text: &[JsonView<'_>]) -> Result<(bool, bool)> {
        let Some((uuid, planned)) = facts
            .uuid
            .as_deref()
            .and_then(|uuid| Some((uuid, plan.messages.get(uuid)?)))
        else {
            return Ok((false, false));
        };
        self.seen_messages.insert(uuid.to_owned());
        match planned {
            Planned::Keep => return Ok((false, false)),
            Planned::Drop => {
                // Its calls and results go with it, and so do their other halves.
                for block in &facts.blocks {
                    self.drop_tools
                        .extend(block.id.iter().chain(&block.tool_use_id).cloned());
                }
                return Ok((true, false));
            }
            _ => {}
        }
        let name = planned.name();
        if !matches!(facts.ty.as_str(), "user" | "assistant" | "attachment") {
            self.problems.push(format!(
                "{uuid}: a {} record is not sent to the model; it takes keep or drop, not {name}",
                facts.ty
            ));
            return Ok((false, false));
        }
        if text.is_empty() {
            self.warnings.push(format!("{uuid}: no text to replace; kept verbatim"));
            return Ok((false, false));
        }
        let mut live = Vec::new();
        for part in text {
            if !is_marker(*part)? {
                live.push(*part);
            }
        }
        if live.is_empty() {
            self.warnings
                .push(format!("{uuid}: already a prune marker; kept verbatim"));
            return Ok((false, false));
        }
        if matches!(planned, Planned::Excerpt { .. }) {
            if facts.ty == "attachment" {
                self.problems.push(format!(
                    "{uuid}: an attachment takes keep, pointer, precis or drop, not excerpt"
                ));
            } else if text.len() != 1 {
                self.problems.push(format!(
                    "{uuid}: an excerpt needs one text payload; this record has {}",
                    text.len()
                ));
            } else {
                self.problems.extend(excerpt_problems(uuid, planned, &string(text[0])?));
            }
        }
        Ok((false, true))
    }

    /// Every main-thread record, in order: the turns [`Unsupported`] is judged on. A user
    /// record with no tool result opens a turn.
    pub(super) fn observe(&mut self, facts: &Facts) {
        if facts.sidechain {
            return;
        }
        let results: Vec<&String> = facts
            .blocks
            .iter()
            .filter(|b| b.ty.as_deref() == Some("tool_result"))
            .filter_map(|b| b.tool_use_id.as_ref())
            .collect();
        match facts.ty.as_str() {
            "user" if results.is_empty() => self.turns.push(Turn::default()),
            "user" => {
                if self.turns.is_empty() {
                    self.turns.push(Turn::default());
                }
                let turn = self.turns.last_mut().expect("a turn");
                turn.results.extend(results.into_iter().cloned());
            }
            "assistant" if facts.blocks.iter().any(|b| b.ty.as_deref() == Some("text")) => {
                if let (Some(turn), Some(uuid)) = (self.turns.last_mut(), &facts.uuid) {
                    let after = !turn.results.is_empty();
                    turn.replies.push((uuid.clone(), after));
                }
            }
            _ => {}
        }
    }

    /// Refuse a plan that named ids the session lacks, passages its payloads lack or an action
    /// a record cannot take; otherwise what the write pass needs ([`Checked`]).
    pub(super) fn finish(mut self, plan: &Plan) -> Result<Checked> {
        let mut unknown: Vec<&String> = plan.results.keys().filter(|id| !self.seen.contains(*id)).collect();
        unknown.sort();
        let mut problems: Vec<String> = unknown
            .into_iter()
            .map(|id| format!("{id}: no tool_result with this id in the session"))
            .collect();
        let mut unknown: Vec<&String> = plan
            .messages
            .keys()
            .filter(|uuid| !self.seen_messages.contains(*uuid))
            .collect();
        unknown.sort();
        problems.extend(
            unknown
                .into_iter()
                .map(|uuid| format!("{uuid}: no record with this uuid in the session")),
        );
        // A result the plan decides one way, in a record (or with a call) it drops.
        let mut contradicted: Vec<&String> = self.planned_results.intersection(&self.drop_tools).collect();
        contradicted.sort();
        problems.extend(contradicted.into_iter().map(|id| {
            format!(
                "{id}: planned as {}, but the plan drops the record that holds it or its call",
                plan.results[id].name()
            )
        }));
        let mut repeated = HashSet::new();
        problems.extend(self.problems.drain(..).filter(|p| repeated.insert(p.clone())));
        if !problems.is_empty() {
            return Err(PlanRefused(problems).into());
        }
        let mut repeated = HashSet::new();
        self.warnings.retain(|w| repeated.insert(w.clone()));

        let unsupported = |id: &String| {
            self.drop_tools.contains(id) || matches!(plan.results.get(id), Some(Planned::Pointer | Planned::Drop))
        };
        let kept = |uuid: &String| !matches!(plan.messages.get(uuid), Some(Planned::Pointer | Planned::Drop));
        let rests_on_nothing = self
            .turns
            .iter()
            .filter(|turn| !turn.results.is_empty() && turn.results.iter().all(unsupported))
            .filter_map(|turn| {
                let (reply, _) = turn.replies.iter().rev().find(|(uuid, after)| *after && kept(uuid))?;
                Some(Unsupported {
                    reply: reply.clone(),
                    results: turn.results.len(),
                })
            })
            .collect();
        Ok(Checked {
            warnings: self.warnings,
            drop_tools: self.drop_tools,
            rests_on_nothing,
        })
    }
}
