//! Conservative enrichment: an index or rounded timestamp alone is never identity.
use super::*;
use std::io::{self, Write};

#[derive(Default)]
pub(super) struct Match {
    pub calls: Vec<String>,
    pub result: Option<String>,
    pub signature: Option<String>,
    pub usage: Option<Usage>,
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
        if let Some(value) = view.get(key)? {
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
pub(super) fn verified(input: &jsonl::Read, order: &StepOrder, path: &Path) -> Result<BTreeMap<u64, Match>> {
    let native = super::native::open(path, false)?;
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
    native.snapshot(0, native.len(), |line| {
        let Some(view) = line.view else {
            return Ok(Flow::Continue);
        };
        if view.get("native_cleared")?.is_some() {
            return Ok(Flow::Continue);
        }
        let Some(index) = view
            .get("step_index")?
            .map(JsonView::as_number)
            .transpose()?
            .flatten()
            .and_then(|n| n.as_u64())
        else {
            return Ok(Flow::Continue);
        };
        if expected.get(&index) != Some(&fingerprint(view)?) {
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
        }
        matches.insert(index, matched);
        Ok(Flow::Continue)
    })?;
    native.check()?;
    input.check()?;
    Ok(matches)
}
