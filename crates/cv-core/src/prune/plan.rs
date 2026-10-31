//! `cv prune --plan`: a caller's judgment of each payload, in place of the size/age heuristics.
//!
//! A plan names tool results by `tool_use_id` (`keep`, `precis`, `excerpt`) and attachment kinds
//! to drop. It is one more source of decisions for the snip path: a planned result is stashed in
//! the sidecar like any snip, its replacement being the usual marker followed by the précis and
//! any kept passages. Every unlisted result stays verbatim. [`PlanCheck`] validates the plan
//! against the session during the decision pass, before anything is staged.
use super::{classify_source, MARKER_PREFIX};
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

/// One planned tool result.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
pub enum Planned {
    /// Verbatim (the default for every unlisted result too).
    Keep,
    /// Marker, then the précis.
    Precis { precis: String },
    /// Marker, the précis (if any), then each kept passage, a literal substring of the payload.
    Excerpt {
        keep: Vec<String>,
        #[serde(default)]
        precis: String,
    },
}

impl Plan {
    pub fn parse(text: &str) -> Result<Plan> {
        Ok(serde_json::from_str(text)?)
    }
    pub(super) fn get(&self, id: Option<&str>) -> Option<&Planned> {
        id.and_then(|id| self.results.get(id))
    }
}

/// A plan the session refuses: unknown ids, passages that are not in their payload, excerpts of
/// image payloads. Nothing was staged or written.
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

/// What the plan changed, for the `--json` report's `plan` object.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PlanReport {
    pub keep: usize,
    pub precis: usize,
    pub excerpt: usize,
    pub attachments_dropped: usize,
    /// Payload bytes the planned replacements and dropped attachment records removed.
    pub bytes_saved: u64,
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
        let mut prefix = String::new();
        content.for_each_str_chunk(|chunk| {
            if prefix.len() < MARKER_PREFIX.len() {
                prefix.extend(chunk.chars().take(MARKER_PREFIX.len()));
            }
            Ok(())
        })?;
        if prefix.starts_with(MARKER_PREFIX) {
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

/// The payload's text, as the model read it: the string, or its text blocks joined by newlines.
/// `Err(kind)` for an image or mixed payload.
pub(super) fn payload_text(content: JsonView<'_>) -> Result<std::result::Result<String, String>> {
    let mut text = String::new();
    if content.is_string()? {
        content.for_each_str_chunk(|chunk| {
            text.push_str(chunk);
            Ok(())
        })?;
        return Ok(Ok(text));
    }
    let kind = classify_source(content)?.0;
    if kind != "text" {
        return Ok(Err(kind));
    }
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

/// The replacement content: the marker, a newline, the précis, then for an excerpt each kept
/// passage in the order it appears in `text`, each after a `[…]` line.
pub(super) fn replacement(marker: &str, planned: &Planned, text: Option<&str>) -> String {
    let mut out = String::from(marker);
    match planned {
        Planned::Keep => {}
        Planned::Precis { precis } => {
            out.push('\n');
            out.push_str(precis);
        }
        Planned::Excerpt { keep, precis } => {
            if !precis.is_empty() {
                out.push('\n');
                out.push_str(precis);
            }
            let text = text.unwrap_or_default();
            let mut passages: Vec<(usize, &String)> = keep
                .iter()
                .map(|p| (text.find(p.as_str()).unwrap_or(usize::MAX), p))
                .collect();
            passages.sort_by_key(|(at, _)| *at);
            for (_, passage) in passages {
                out.push_str("\n[…]\n");
                out.push_str(passage);
            }
        }
    }
    out
}

/// The plan checked against every record's results during the decision pass.
#[derive(Default)]
pub(super) struct PlanCheck {
    seen: HashSet<String>,
    problems: Vec<String>,
    pub warnings: Vec<String>,
}

impl PlanCheck {
    /// One `tool_result` block of a user record. Returns `(planned replacement applies, planned
    /// keep)`.
    pub(super) fn result(&mut self, plan: &Plan, id: &str, content: Option<JsonView<'_>>) -> Result<(bool, bool)> {
        let Some(planned) = plan.results.get(id) else {
            return Ok((false, false));
        };
        self.seen.insert(id.to_owned());
        if *planned == Planned::Keep {
            return Ok((false, true));
        }
        if let Effect::Verbatim(why) = effect(id, content)? {
            self.warnings.extend(why);
            return Ok((false, false));
        }
        if let (Planned::Excerpt { keep, .. }, Some(content)) = (planned, content) {
            match payload_text(content)? {
                Err(kind) => self
                    .problems
                    .push(format!("{id}: an excerpt needs a text payload; this one is {kind}")),
                Ok(text) => {
                    for passage in keep.iter().filter(|p| !text.contains(p.as_str())) {
                        let head: String = passage.chars().take(80).collect();
                        let more = if passage.chars().count() > 80 { "…" } else { "" };
                        self.problems
                            .push(format!("{id}: kept passage is not in the payload: {head:?}{more}"));
                    }
                }
            }
        }
        Ok((true, false))
    }

    /// Refuse a plan that named ids the session lacks or passages its payloads lack; otherwise
    /// the warnings (results kept verbatim despite the plan).
    pub(super) fn finish(mut self, plan: &Plan) -> Result<Vec<String>> {
        let mut unknown: Vec<&String> = plan.results.keys().filter(|id| !self.seen.contains(*id)).collect();
        unknown.sort();
        let mut problems: Vec<String> = unknown
            .into_iter()
            .map(|id| format!("{id}: no tool_result with this id in the session"))
            .collect();
        let mut repeated = HashSet::new();
        problems.extend(self.problems.drain(..).filter(|p| repeated.insert(p.clone())));
        if !problems.is_empty() {
            return Err(PlanRefused(problems).into());
        }
        let mut repeated = HashSet::new();
        self.warnings.retain(|w| repeated.insert(w.clone()));
        Ok(self.warnings)
    }
}
