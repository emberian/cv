//! Conservative enrichment: an index or rounded timestamp alone is never identity.
use super::*;
use std::io::{self, Write};

#[derive(Default)]
pub(super) struct Match {
    pub calls: Vec<String>,
    pub result: Option<String>,
    pub signature: Option<String>,
    pub usage: Option<Usage>,
    pub planner_details: Option<Value>,
    /// The permission request and its answer, which the export does not carry.
    pub interaction: Option<Value>,
    /// The native revision of a result whose call a verified planner step names.
    pub revised: Option<Revised>,
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
    /// Steps the export never wrote: cancelled tools, upstream errors, history before a
    /// re-created log, and steps newer than its last flush.
    pub native_only: StepOrder,
    pub rejected: bool,
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
fn fingerprint(view: JsonView<'_>) -> Result<blake3::Hash> {
    let mut hash = Fingerprint(blake3::Hasher::new());
    let user = view.get("type")?.map(JsonView::string).transpose()?.flatten().as_deref() == Some("USER_INPUT");
    for key in ["type", "content", "thinking"] {
        hash.write_all(key.as_bytes())?;
        let mut value = view.get(key)?;
        // The export wraps the typed request in model-facing metadata; the store keeps
        // the request alone, as one item. Compare the request, never the wrapper.
        if key == "content" && user {
            let request = match value {
                Some(content) => content
                    .string()?
                    .map(|text| super::exported_user_request(&text).map(str::to_owned).unwrap_or(text)),
                None => match view.get("native_content")?.map(JsonView::array).transpose()?.flatten() {
                    Some(mut parts) => match (parts.next().transpose()?, parts.next().transpose()?) {
                        (Some(only), None) => only.string()?,
                        _ => None,
                    },
                    None => None,
                },
            };
            if let Some(request) = request {
                hash.write_all(request.as_bytes())?;
            }
            hash.write_all(&[0])?;
            continue;
        }
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
    if let Some(ts) = view
        .get("created_at")?
        .map(JsonView::string)
        .transpose()?
        .flatten()
        .as_deref()
        .and_then(super::super::parse_ts)
    {
        hash.write_all(&ts.timestamp().to_le_bytes())?;
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
pub(super) fn verified(
    input: &jsonl::Read,
    order: &StepOrder,
    path: &Path,
    complete: bool,
    include_native_only: bool,
) -> Result<Verified> {
    let native = super::native::open(path, complete)?;
    let mut expected = BTreeMap::new();
    let mut records = input.records();
    for (index, (at, end)) in &order.steps {
        records.read(*at, end - at, |view| {
            expected.insert(*index, fingerprint(view)?);
            Ok(Flow::Continue)
        })?;
    }
    let mut matches = BTreeMap::new();
    let mut known_calls = BTreeSet::new();
    let mut native_only = StepOrder::default();
    let mut newest_comparable = None;
    native.snapshot(0, native.len(), |line| {
        let Some(view) = line.view else {
            return Ok(Flow::Continue);
        };
        let Some(index) = view
            .get("step_index")?
            .map(JsonView::as_number)
            .transpose()?
            .flatten()
            .and_then(|n| n.as_u64())
        else {
            return Ok(Flow::Continue);
        };
        if !expected.contains_key(&index) {
            if include_native_only {
                native_only.ingest(&line)?;
            }
            return Ok(Flow::Continue);
        }
        if view.get("native_cleared")?.is_some() {
            return Ok(Flow::Continue);
        }
        let equal = expected.get(&index) == Some(&fingerprint(view)?);
        let kind = view.get("type")?.map(JsonView::string).transpose()?.flatten();
        // A request the export decorated with mentions cannot be compared; only a
        // planner body that differs is evidence of another revision.
        if kind.as_deref() == Some("PLANNER_RESPONSE") || (kind.as_deref() == Some("USER_INPUT") && equal) {
            newest_comparable = Some(equal);
        }
        if !equal {
            if kind.as_deref() == Some("GENERIC") {
                if let Some(revised) = revision(view, &known_calls)? {
                    matches.insert(index, revised);
                }
            }
            return Ok(Flow::Continue);
        }
        let mut matched = Match::default();
        if let Some(calls) = view.get("tool_calls")?.map(JsonView::array).transpose()?.flatten() {
            for call in calls {
                let Some(id) = call?
                    .get("id")?
                    .map(JsonView::string)
                    .transpose()?
                    .flatten()
                    .filter(|id| !id.is_empty())
                else {
                    return Ok(Flow::Continue);
                };
                matched.calls.push(id);
            }
            known_calls.extend(matched.calls.iter().cloned());
        }
        matched.result = view
            .get("native_tool_id")?
            .map(JsonView::string)
            .transpose()?
            .flatten()
            .filter(|id| known_calls.contains(id));
        matched.signature = view
            .get("thinking_signature")?
            .map(JsonView::string)
            .transpose()?
            .flatten();
        if let Some(fields) = Fields::parse(view, STEP_KEYS)? {
            matched.usage = parse_step_usage(&fields)?;
            matched.planner_details = fields.value("native_planner_details")?;
            matched.interaction = fields.value("native_interaction")?;
        }
        matches.insert(index, matched);
        Ok(Flow::Continue)
    })?;
    native.check()?;
    input.check()?;
    let rejected = !native_only.steps.is_empty() && newest_comparable != Some(true);
    if rejected {
        native_only.steps.clear();
    }
    Ok(Verified {
        matches,
        native,
        native_only,
        rejected,
    })
}

/// A result row whose body differs from the export is still the same call when the
/// store names a call ID that a verified planner step issued.
fn revision(view: JsonView<'_>, known_calls: &BTreeSet<String>) -> Result<Option<Match>> {
    let Some(id) = view
        .get("native_tool_id")?
        .map(JsonView::string)
        .transpose()?
        .flatten()
        .filter(|id| known_calls.contains(id))
    else {
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
}
