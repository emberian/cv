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
}
pub(super) struct Verified {
    pub matches: BTreeMap<u64, Match>,
    pub native: jsonl::Read,
    pub tail: StepOrder,
    pub rejected_tail: bool,
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
    include_tail: bool,
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
    let mut tail = StepOrder::default();
    let max_exported = order.steps.keys().next_back().copied();
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
        if include_tail && max_exported.is_some_and(|max| index > max) {
            tail.ingest(&line)?;
            return Ok(Flow::Continue);
        }
        if view.get("native_cleared")?.is_some() {
            return Ok(Flow::Continue);
        }
        let equal = expected.get(&index) == Some(&fingerprint(view)?);
        if expected.contains_key(&index)
            && matches!(
                view.get("type")?
                    .map(JsonView::string)
                    .transpose()?
                    .flatten()
                    .as_deref(),
                Some("USER_INPUT" | "PLANNER_RESPONSE")
            )
        {
            newest_comparable = Some(equal);
        }
        if !equal {
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
        }
        matches.insert(index, matched);
        Ok(Flow::Continue)
    })?;
    native.check()?;
    input.check()?;
    let rejected_tail = !tail.steps.is_empty() && newest_comparable != Some(true);
    if rejected_tail {
        tail.steps.clear();
    }
    Ok(Verified {
        matches,
        native,
        tail,
        rejected_tail,
    })
}
