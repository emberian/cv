//! Conservative enrichment: an index or rounded timestamp alone is never identity.
//!
//! The export is the model-facing rendering of a native step: a tool result gains a
//! `Created At:`/`Completed At:` header, a prompt and a system message gain tags and
//! trailing metadata, a checkpoint gains resumption prose. A planner step is exported
//! verbatim and is compared exactly. Every other type is the same revision when its
//! native body is one line-aligned block of the exported content.
use super::*;
use std::io::{self, Write};

#[derive(Default)]
pub(super) struct Match {
    pub calls: Vec<String>,
    pub result: Option<String>,
    pub signature: Option<String>,
    pub usage: Option<Usage>,
    pub planner_details: Option<Value>,
    pub checkpoint_details: Option<Value>,
    pub task_details: Option<Value>,
    pub structured_result: Option<Value>,
    /// The permission request and its answer, which the export does not carry.
    pub interaction: Option<Value>,
    /// The native revision of a result whose body is not the exported one.
    pub revised: Option<Revised>,
    /// The typed result of the verified revision; not kept for a result matched by call alone.
    pub payload: Option<Value>,
}
/// The writer keeps revising a tool step after exporting it: a backgrounded command's
/// acknowledgement stays `RUNNING` in the log while the store records how it ended.
pub(super) struct Revised {
    pub status: String,
    pub task_details: Option<Value>,
}
pub(super) struct Verified {
    pub matches: BTreeMap<u64, Match>,
    pub native: jsonl::Read,
    /// Native steps the export lacks, each beside verified exported steps.
    pub native_only: StepOrder,
    /// Every comparable overlapping step matched, with at least one verified overlap.
    pub overlaps_verified: bool,
    /// A native step the export lacks could not be placed beside verified history.
    pub unreconciled: bool,
    /// Some native step holds a field no decoder reads.
    pub unread_fields: bool,
    /// Verified exported steps whose linked media file is gone while the store kept
    /// the bytes: each is read from its native row (it is in `native_only`).
    pub recovered_media: BTreeSet<u64>,
}

const HEAD: usize = 32;
/// Partial matches hashed at once; a body repeated on more lines than this stays unverified.
const CANDIDATES: usize = 16;

struct Body {
    len: u64,
    hash: blake3::Hash,
    head: Vec<u8>,
}
struct Print {
    exact: blake3::Hash,
    content: Option<Body>,
    error: Option<Body>,
}
struct NativeStep {
    at: u64,
    end: u64,
    kind: Option<String>,
    second: Option<i64>,
    /// None for a cleared row: nothing remains to compare.
    print: Option<Print>,
}

struct Fingerprint(blake3::Hasher);
impl Write for Fingerprint {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn string(view: JsonView<'_>, key: &str) -> Result<Option<String>> {
    Ok(view.get(key)?.map(JsonView::string).transpose()?.flatten())
}
fn index(view: JsonView<'_>) -> Result<Option<u64>> {
    Ok(view
        .get("step_index")?
        .map(JsonView::as_number)
        .transpose()?
        .flatten()
        .and_then(|n| n.as_u64()))
}
fn second(view: JsonView<'_>) -> Result<Option<i64>> {
    Ok(string(view, "created_at")?
        .as_deref()
        .and_then(super::super::parse_ts)
        .map(|ts| ts.timestamp()))
}

fn fingerprint(view: JsonView<'_>) -> Result<blake3::Hash> {
    let mut hash = Fingerprint(blake3::Hasher::new());
    for key in ["type", "content", "thinking"] {
        hash.write_all(key.as_bytes())?;
        let mut value = view.get(key)?;
        // The native user shape stores text as items; a single item is exactly
        // the same body as the export's content string, without guessing joins.
        if key == "content" && value.is_none() {
            if let Some(mut parts) = view.get("native_content")?.map(JsonView::array).transpose()?.flatten() {
                if let Some(first) = parts.next() {
                    let first = first?;
                    if parts.next().is_none() {
                        value = Some(first);
                    }
                }
            }
        }
        if let Some(value) = value {
            value.write_canonical(&mut hash)?;
        }
        hash.write_all(&[0])?;
    }
    // Exported timestamps are rounded; combine their seconds with the entire payload.
    if let Some(second) = second(view)? {
        hash.write_all(&second.to_le_bytes())?;
    }
    if let Some(calls) = view.get("tool_calls")?.map(JsonView::array).transpose()?.flatten() {
        for call in calls {
            let call = call?;
            if let Some(name) = call.get("name")? {
                name.write_canonical(&mut hash)?;
            }
            hash.write_all(&[0])?;
            if let Some(args) = call.get("arguments")? {
                args.encoded_argument()?.write_canonical(&mut hash)?;
            } else if let Some(args) = call.get("args")? {
                args.write_canonical(&mut hash)?;
            }
            hash.write_all(&[0])?;
        }
    }
    Ok(hash.0.finalize())
}

/// A text an exported rendering must contain, from string values read in order.
fn body<'a>(parts: impl IntoIterator<Item = Result<JsonView<'a>>>) -> Result<Option<Body>> {
    let mut hasher = blake3::Hasher::new();
    let mut len = 0u64;
    let mut head = Vec::new();
    for part in parts {
        part?.for_each_str_chunk(|chunk| {
            hasher.update(chunk.as_bytes());
            len += chunk.len() as u64;
            let take = (HEAD - head.len()).min(chunk.len());
            head.extend_from_slice(&chunk.as_bytes()[..take]);
            Ok(())
        })?;
    }
    Ok((len > 0).then(|| Body {
        len,
        hash: hasher.finalize(),
        head,
    }))
}

/// The step's content, or a prompt's text items in order (the writer's `user_response`
/// is their concatenation).
fn native_content(view: JsonView<'_>) -> Result<Option<Body>> {
    if let Some(content) = view.get("content")? {
        return body([Ok(content)]);
    }
    match view.get("native_content")?.map(JsonView::array).transpose()?.flatten() {
        Some(parts) => body(parts),
        None => Ok(None),
    }
}

/// One line start of the exported text, compared with the body as bytes arrive.
#[derive(Default)]
struct Candidate {
    fed: u64,
    hasher: Option<blake3::Hasher>,
}
impl Candidate {
    /// Whether the candidate can still match; `found` is set when it has.
    fn feed(&mut self, mut bytes: &[u8], body: &Body, found: &mut bool) -> bool {
        let head = body.head.len() as u64;
        if self.fed < head {
            let take = ((head - self.fed) as usize).min(bytes.len());
            if bytes[..take] != body.head[self.fed as usize..][..take] {
                return false;
            }
            self.fed += take as u64;
            bytes = &bytes[take..];
            if self.fed < head {
                return true;
            }
            self.hasher.insert(blake3::Hasher::new()).update(&body.head);
        }
        let take = (body.len - self.fed).min(bytes.len() as u64) as usize;
        if let Some(hasher) = &mut self.hasher {
            hasher.update(&bytes[..take]);
        }
        self.fed += take as u64;
        match bytes.get(take) {
            // The body has to end its line.
            Some(after) => {
                *found |= *after == b'\n' && self.complete(body);
                false
            }
            None => true,
        }
    }
    fn complete(&self, body: &Body) -> bool {
        self.fed == body.len
            && self
                .hasher
                .as_ref()
                .is_some_and(|hasher| hasher.finalize() == body.hash)
    }
}

/// Whether `text` is `prefix + body + suffix`, the prefix empty or ending a line and the
/// suffix empty or starting one. The text is streamed; no part of it is held.
fn embeds(text: JsonView<'_>, body: &Body) -> Result<bool> {
    let mut live: Vec<Candidate> = Vec::new();
    let mut line_start = true;
    let mut found = false;
    let is_string = text.for_each_str_chunk(|chunk| {
        let bytes = chunk.as_bytes();
        live.retain_mut(|candidate| candidate.feed(bytes, body, &mut found));
        let mut at = 0;
        while at < bytes.len() {
            if line_start && live.len() < CANDIDATES {
                let mut candidate = Candidate::default();
                if candidate.feed(&bytes[at..], body, &mut found) {
                    live.push(candidate);
                }
            }
            at += bytes[at..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len() - at, |newline| newline + 1);
            line_start = bytes[at - 1] == b'\n';
        }
        if found {
            return Err(crate::stream::Stop.into());
        }
        Ok(())
    });
    if crate::stream::stopped(is_string)? == Some(false) {
        return Ok(false);
    }
    Ok(found || live.iter().any(|candidate| candidate.complete(body)))
}

/// `Some(equal)` when the exported and native rows at one index can be compared.
fn compare(exported: JsonView<'_>, kind: Option<&str>, native: &NativeStep, print: &Print) -> Result<Option<bool>> {
    if fingerprint(exported)? == print.exact {
        return Ok(Some(true));
    }
    if kind != native.kind.as_deref() || kind == Some("PLANNER_RESPONSE") || second(exported)? != native.second {
        return Ok(Some(false));
    }
    // A failed step exports the writer's error text beside, or instead of, its content.
    for (key, body) in [("content", &print.content), ("error", &print.error)] {
        if let (Some(body), Some(text)) = (body, exported.get(key)?) {
            if embeds(text, body)? {
                return Ok(Some(true));
            }
        }
    }
    // A step exported while it ran is an earlier revision of the finished native row,
    // and a request the export decorated (expanded mentions, several items) cannot be
    // compared: neither is evidence of a divergent history, nor an identity.
    let running = matches!(
        string(exported, "status")?.as_deref(),
        Some("RUNNING" | "PENDING" | "GENERATING" | "WAITING" | "QUEUED")
    );
    Ok((!running && kind != Some("USER_INPUT")).then_some(false))
}

pub(super) fn verified(
    input: &jsonl::Read,
    order: &StepOrder,
    path: &Path,
    complete: bool,
    include_native_only: bool,
) -> Result<Verified> {
    let native = super::native::open(path, complete)?;
    let mut steps = BTreeMap::new();
    let mut unread_fields = false;
    native.snapshot(0, native.len(), |line| {
        let Some(view) = line.view else {
            return Ok(Flow::Continue);
        };
        unread_fields |= view.get("native_unsupported")?.is_some();
        let Some(index) = index(view)? else {
            return Ok(Flow::Continue);
        };
        let print = if view.get("native_cleared")?.is_some() {
            None
        } else {
            Some(Print {
                exact: fingerprint(view)?,
                content: native_content(view)?,
                error: body(view.get("error")?.map(Ok))?,
            })
        };
        steps.insert(
            index,
            NativeStep {
                at: line.at,
                end: line.end,
                kind: string(view, "type")?,
                second: second(view)?,
                print,
            },
        );
        Ok(Flow::Continue)
    })?;
    let mut verdicts = BTreeMap::new();
    // Exported tool results at a native tool result's index and second.
    let mut results = BTreeSet::new();
    let mut records = input.records();
    for (index, (at, end)) in &order.steps {
        let Some(step) = steps.get(index) else {
            continue;
        };
        let verdict = records.read(*at, end - at, |view| {
            let kind = string(view, "type")?;
            if kind.as_deref() == Some("GENERIC") && step.kind == kind && second(view)? == step.second {
                results.insert(*index);
            }
            match &step.print {
                Some(print) => compare(view, kind.as_deref(), step, print),
                None => Ok(None),
            }
        })?;
        if let Some(equal) = verdict {
            verdicts.insert(*index, equal);
        }
    }
    let overlaps_verified = verdicts.values().any(|&v| v) && verdicts.values().all(|&v| v);
    let mut accepted = BTreeSet::new();
    let mut unreconciled = false;
    for index in steps.keys().filter(|index| !order.steps.contains_key(index)) {
        let below = verdicts.range(..index).next_back().map(|(_, equal)| *equal);
        let above = verdicts.range(index..).next().map(|(_, equal)| *equal);
        // The nearest comparable exported step on each side must be this native revision;
        // one side may have none (a native head or tail), never both.
        let beside_verified = below != Some(false) && above != Some(false) && (below.is_some() || above.is_some());
        if include_native_only && beside_verified {
            accepted.insert(*index);
        } else if include_native_only {
            unreconciled = true;
        }
    }
    let mut matches = BTreeMap::new();
    let mut known_calls = BTreeSet::new();
    let mut native_only = StepOrder::default();
    let mut native_records = native.records();
    let mut recovered_media = BTreeSet::new();
    for index in &order.missing_media {
        let Some(step) = steps.get(index).filter(|_| verdicts.get(index) == Some(&true)) else {
            continue;
        };
        let kept = native_records.read(step.at, step.end - step.at, |view| {
            let media = view.get("media")?.map(JsonView::array).transpose()?.flatten();
            Ok(view.get("native_missing_attachments")?.is_none()
                && media.is_some_and(|mut items| items.next().is_some()))
        })?;
        if kept {
            recovered_media.insert(*index);
        }
    }
    for (index, step) in &steps {
        if accepted.contains(index) || recovered_media.contains(index) {
            native.snapshot(step.at, step.end, |line| {
                native_only.ingest(&line)?;
                Ok(Flow::Continue)
            })?;
            continue;
        }
        if verdicts.get(index) != Some(&true) {
            // The call a result answers is a fact of its step, whichever revision of its
            // body was exported: an id issued by a verified planner step is kept.
            if results.contains(index) {
                let revision = native_records.read(step.at, step.end - step.at, |view| {
                    let Some(id) = string(view, "native_tool_id")?.filter(|id| known_calls.contains(id)) else {
                        return Ok(None);
                    };
                    let Some(fields) = Fields::parse(view, STEP_KEYS)? else {
                        return Ok(None);
                    };
                    Ok(Some(Match {
                        result: Some(id),
                        interaction: fields.value("native_interaction")?,
                        revised: Some(Revised {
                            status: fields.string("status")?.unwrap_or_default(),
                            task_details: fields.value("native_task_details")?,
                        }),
                        ..Match::default()
                    }))
                })?;
                if let Some(revision) = revision {
                    matches.insert(*index, revision);
                }
            }
            continue;
        }
        let matched = native_records.read(step.at, step.end - step.at, |view| {
            let mut matched = Match::default();
            if let Some(calls) = view.get("tool_calls")?.map(JsonView::array).transpose()?.flatten() {
                for call in calls {
                    let Some(id) = string(call?, "id")?.filter(|id| !id.is_empty()) else {
                        return Ok(None);
                    };
                    matched.calls.push(id);
                }
                known_calls.extend(matched.calls.iter().cloned());
            }
            matched.result = string(view, "native_tool_id")?.filter(|id| known_calls.contains(id));
            matched.signature = string(view, "thinking_signature")?;
            if let Some(fields) = Fields::parse(view, STEP_KEYS)? {
                matched.usage = parse_step_usage(&fields)?;
                matched.planner_details = fields.value("native_planner_details")?;
                matched.checkpoint_details = fields.value("native_checkpoint_details")?;
                matched.task_details = fields.value("native_task_details")?;
                matched.structured_result = fields.value("native_structured_result")?;
                matched.interaction = fields.value("native_interaction")?;
                matched.payload = fields.value("native_payload")?;
            }
            Ok(Some(matched))
        })?;
        if let Some(matched) = matched {
            matches.insert(*index, matched);
        }
    }
    native.check()?;
    input.check()?;
    Ok(Verified {
        matches,
        native,
        native_only,
        overlaps_verified,
        unreconciled,
        unread_fields,
        recovered_media,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedded(text: &str, native: &str) -> bool {
        let native = json!(native);
        let body = body([Ok(JsonView::from(&native))]).unwrap().unwrap();
        let value = json!(text);
        embeds(JsonView::from(&value), &body).unwrap()
    }

    #[test]
    fn a_body_is_embedded_only_as_a_line_aligned_block() {
        let header = "Created At: 2026-11-14T10:00:01Z\nCompleted At: 2026-11-14T10:00:02Z\n";
        assert!(embedded(&format!("{header}1"), "1"));
        assert!(embedded("exact", "exact"));
        assert!(embedded(
            "<USER_REQUEST>\nfix it\n</USER_REQUEST>\n<ADDITIONAL_METADATA>",
            "fix it"
        ));
        // The digit inside the header, a longer line and a later revision are not this body.
        assert!(!embedded(header, "1"));
        assert!(!embedded(&format!("{header}10"), "1"));
        assert!(!embedded(&format!("{header}line one"), "line one\nline two"));
        assert!(!embedded(&format!("{header}other"), "output"));
        // Neither the wrapper nor the body has a size the comparison depends on.
        let large = "x".repeat(300_000);
        let preamble = "resumption prose\n".repeat(20_000);
        assert!(embedded(&format!("{preamble}{large}\ntrailer"), &large));
        assert!(!embedded(&format!("{preamble}{large}y"), &large));
        // A body that begins on many lines is found on the one where it is whole.
        let start = "same start of thirty-two bytes..";
        assert!(embedded(
            &format!("{}{start}!", format!("{start}\n").repeat(8)),
            &format!("{start}!")
        ));
    }
}
