//! Rewrite only named wire fields. Unknown fields retain their original encoding.
use super::super::native::read_varint;
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
pub(super) struct Field<'a> {
    pub number: u32,
    pub wire: u8,
    pub value: &'a [u8],
    pub raw: &'a [u8],
}

pub(super) fn walk<'a>(data: &'a [u8], mut f: impl FnMut(Field<'a>) -> Result<()>) -> Result<()> {
    let mut at = 0;
    while at < data.len() {
        let start = at;
        let tag = read_varint(data, &mut at)?;
        ensure!((1..=0x1fff_ffff).contains(&(tag >> 3)), "invalid protobuf tag");
        let wire = (tag & 7) as u8;
        let mut offset = at;
        match wire {
            0 => {
                read_varint(data, &mut at)?;
            }
            2 => {
                let len = usize::try_from(read_varint(data, &mut at)?)?;
                offset = at;
                at = at.checked_add(len).context("protobuf length overflow")?;
            }
            1 | 5 => {
                at = at
                    .checked_add(if wire == 1 { 8 } else { 4 })
                    .context("protobuf offset overflow")?
            }
            _ => anyhow::bail!("unsupported protobuf wire type {wire}"),
        }
        f(Field {
            number: (tag >> 3) as u32,
            wire,
            value: data.get(offset..at).context("short protobuf field")?,
            raw: &data[start..at],
        })?;
    }
    Ok(())
}

fn varint(buf: &mut Vec<u8>, mut n: u64) {
    while n >= 128 {
        buf.push(n as u8 | 128);
        n >>= 7;
    }
    buf.push(n as u8);
}
pub(in super::super) fn scalar(buf: &mut Vec<u8>, n: u32, v: u64) {
    varint(buf, u64::from(n) << 3);
    varint(buf, v);
}
pub(in super::super) fn bytes(buf: &mut Vec<u8>, n: u32, v: &[u8]) {
    varint(buf, (u64::from(n) << 3) | 2);
    varint(buf, v.len() as u64);
    buf.extend(v);
}

pub(super) fn unique(data: &[u8], number: u32, wire: u8) -> Result<Option<Vec<u8>>> {
    let mut value = None;
    walk(data, |f| {
        if f.number == number {
            ensure!(
                f.wire == wire && value.is_none(),
                "invalid or repeated protobuf field {number}"
            );
            value = Some(f.value.to_vec());
        }
        Ok(())
    })?;
    Ok(value)
}
pub(super) fn nested(data: &[u8], number: u32) -> Result<Vec<u8>> {
    unique(data, number, 2)?.with_context(|| format!("missing protobuf field {number}"))
}
pub(super) fn integer(data: &[u8], number: u32) -> Result<u64> {
    unique(data, number, 0)?.map_or(Ok(0), |v| read_varint(&v, &mut 0))
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Reference<'a> {
    pub trajectory: &'a str,
    pub step: u64,
    pub turn: u64,
    pub cascade: &'a str,
}
impl Reference<'_> {
    pub fn verify(&self, md: &[u8]) -> Result<()> {
        let r = nested(md, 20)?;
        ensure!(
            nested(&r, 1)? == self.trajectory.as_bytes()
                && nested(&r, 4)? == self.cascade.as_bytes()
                && integer(&r, 2)? == self.step
                && integer(&r, 3)? == self.turn,
            "inconsistent native step reference at {}",
            self.step
        );
        Ok(())
    }
    pub fn encode(&self, old: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        walk(old, |f| {
            if !(1..=4).contains(&f.number) {
                out.extend(f.raw)
            }
            Ok(())
        })?;
        bytes(&mut out, 1, self.trajectory.as_bytes());
        if self.step != 0 {
            scalar(&mut out, 2, self.step)
        }
        if self.turn != 0 {
            scalar(&mut out, 3, self.turn)
        }
        bytes(&mut out, 4, self.cascade.as_bytes());
        Ok(out)
    }
}
pub(super) fn turn(md: &[u8]) -> Result<u64> {
    integer(&nested(md, 20)?, 3)
}

pub(super) fn transform(data: &[u8], mut edit: impl FnMut(Field<'_>) -> Result<Option<Vec<u8>>>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    walk(data, |f| {
        if let Some(v) = edit(f)? {
            ensure!(f.wire == 2, "expected length-delimited field {}", f.number);
            bytes(&mut out, f.number, &v)
        } else {
            out.extend(f.raw)
        }
        Ok(())
    })?;
    Ok(out)
}
pub(super) fn metadata(data: &[u8], new: &Reference<'_>) -> Result<Vec<u8>> {
    transform(data, |f| {
        if f.number == 20 {
            new.encode(f.value).map(Some)
        } else {
            Ok(None)
        }
    })
}
pub(super) fn task(data: &[u8], old: &str, new: &str) -> Result<Vec<u8>> {
    transform(data, |f| {
        if f.number == 1 {
            if let Some(tail) = f
                .value
                .strip_prefix(old.as_bytes())
                .filter(|v| v.is_empty() || v.starts_with(b"/task-"))
            {
                return Ok(Some([new.as_bytes(), tail].concat()));
            }
        }
        Ok(None)
    })
}
pub(super) fn payload(data: &[u8], new: &Reference<'_>, old: &str) -> Result<Vec<u8>> {
    transform(data, |f| match f.number {
        5 => metadata(f.value, new).map(Some),
        148 => task(f.value, old, new.cascade).map(Some),
        _ => Ok(None),
    })
}
pub(super) fn trajectory_standalone(data: &[u8], old: &str, new: &str) -> Result<(Vec<u8>, bool)> {
    let mut stripped_parent = false;
    let mut out = Vec::new();
    walk(data, |f| {
        if f.number == 5 {
            stripped_parent = true;
            return Ok(());
        }
        if f.number == 6 {
            ensure!(f.wire == 2, "expected length-delimited field 6");
            if f.value != old.as_bytes() {
                stripped_parent = true;
            }
            bytes(&mut out, 6, new.as_bytes());
            return Ok(());
        }
        out.extend(f.raw);
        Ok(())
    })?;
    Ok((out, stripped_parent))
}

/// Compare all payload fields outside the two metadata carriers to the source,
/// including signatures, repeated user bodies and literal UUIDs.
pub(super) fn body_digest(data: &[u8]) -> Result<blake3::Hash> {
    let mut hash = blake3::Hasher::new();
    walk(data, |f| {
        if !matches!(f.number, 5 | 148) {
            hash.update(f.raw);
        }
        Ok(())
    })?;
    Ok(hash.finalize())
}

pub(super) fn generation(data: &[u8], indices: &BTreeMap<u64, u64>) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    let mut complete = true;
    let mut seen = false;
    walk(data, |f| {
        if f.number != 2 {
            out.extend(f.raw);
            return Ok(());
        }
        ensure!(matches!(f.wire, 0 | 2), "invalid generation step indices");
        let mut at = 0;
        let mut packed = Vec::new();
        while at < f.value.len() {
            seen = true;
            let old = read_varint(f.value, &mut at)?;
            if let Some(&new) = indices.get(&old) {
                if f.wire == 0 {
                    scalar(&mut out, 2, new)
                } else {
                    varint(&mut packed, new)
                }
            } else {
                complete = false
            }
        }
        if f.wire == 2 {
            bytes(&mut out, 2, &packed)
        }
        Ok(())
    })?;
    Ok((complete && seen).then_some(out))
}

#[derive(Debug)]
pub(super) struct SnippedGeneric {
    pub call_id: String,
    pub tool_name: String,
    pub input: Value,
    pub original: String,
    pub bytes_saved: u64,
}

fn last_string(data: &[u8], number: u32) -> Result<Option<String>> {
    let mut found = None;
    walk(data, |f| {
        if f.number == number && f.wire == 2 {
            found = Some(String::from_utf8_lossy(f.value).into_owned());
        }
        Ok(())
    })?;
    Ok(found)
}

/// Extract non-empty tool-call IDs emitted by a `PLANNER_RESPONSE` (`step_payload.20.7.*.1`).
pub(super) fn planner_tool_ids(body: &[u8]) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let Some(planner) = unique(body, 20, 2)? else {
        return Ok(ids);
    };
    walk(&planner, |f| {
        if f.number == 7 && f.wire == 2 {
            if let Some(id) = last_string(f.value, 1)?.filter(|s| !s.is_empty()) {
                ids.push(id);
            }
        }
        Ok(())
    })?;
    Ok(ids)
}

/// Extract the tool-call ID (`metadata.4.1`) carried by a tool-result step.
pub(super) fn step_tool_id(md: &[u8], body: &[u8]) -> Result<Option<String>> {
    let carrier = unique(body, 5, 2)?;
    let meta = carrier.as_deref().unwrap_or(md);
    let Some(call) = unique(meta, 4, 2)? else {
        return Ok(None);
    };
    Ok(last_string(&call, 1)?.filter(|s| !s.is_empty()))
}

fn tool_call_meta(md: &[u8], body: &[u8], generic: &[u8]) -> Result<(Option<String>, String, Value)> {
    let carrier = unique(body, 5, 2)?;
    let meta = carrier.as_deref().unwrap_or(md);
    if let Some(call) = unique(meta, 4, 2)? {
        let id = last_string(&call, 1)?.filter(|s| !s.is_empty());
        let name = last_string(&call, 2)?.unwrap_or_default();
        let args = last_string(&call, 3)?
            .or(last_string(&call, 4)?)
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());
        if let Some(args) = args {
            return Ok((id, name, args));
        }
        let fallback = generic_map_json(generic)?;
        return Ok((id, name, fallback));
    }
    Ok((None, String::new(), generic_map_json(generic)?))
}

fn generic_map_json(generic: &[u8]) -> Result<Value> {
    let mut map = serde_json::Map::new();
    walk(generic, |f| {
        if f.number == 1 && f.wire == 2 {
            if let Some(k) = last_string(f.value, 1)? {
                let v = last_string(f.value, 2)?.unwrap_or_default();
                map.insert(k, Value::String(v));
            }
        }
        Ok(())
    })?;
    Ok(if map.is_empty() {
        Value::Null
    } else {
        Value::Object(map)
    })
}

/// Strip bloated `30.26` `CortexCheckpointSection`s (and optionally replace `30.5` summary and
/// reset stale snapshot lists when curating a fresh anchor).
pub(super) fn checkpoint_prune(
    checkpoint: &[u8],
    replace_summary: Option<&str>,
    sludge_min_bytes: usize,
) -> Result<(Vec<u8>, u64, usize)> {
    let mut out = Vec::new();
    let mut sections_stripped = 0usize;
    let mut wrote_summary = false;
    if let Some(summary) = replace_summary {
        walk(checkpoint, |f| {
            match f.number {
                // Reset bounds & subagent count below; replace summary cleanly once.
                1 | 5 | 11 | 12 | 24 | 27 => {}
                // Strip accumulated sludge sections, dead subagent/task/artifact snapshots, and stale requests.
                14 | 20 | 21 | 26 | 30 | 31 => {
                    if f.number == 26 {
                        sections_stripped += 1;
                    }
                }
                _ => out.extend(f.raw),
            }
            Ok(())
        })?;
        let mut framed = Vec::new();
        scalar(&mut framed, 1, 1);
        bytes(&mut framed, 5, summary.as_bytes());
        scalar(&mut framed, 12, 0);
        scalar(&mut framed, 24, 1);
        scalar(&mut framed, 27, 0);
        framed.extend(out);
        let saved = checkpoint.len().saturating_sub(framed.len()) as u64;
        return Ok((framed, saved, sections_stripped));
    }

    walk(checkpoint, |f| {
        if f.number == 26 && f.raw.len() > sludge_min_bytes {
            sections_stripped += 1;
            return Ok(());
        }
        if f.number == 5 {
            wrote_summary = true;
        }
        out.extend(f.raw);
        Ok(())
    })?;
    let _ = wrote_summary;
    let saved = checkpoint.len().saturating_sub(out.len()) as u64;
    Ok((out, saved, sections_stripped))
}

/// Materialize step `0` as a clean `step_type = 23` (`CHECKPOINT`) carrying `summary`, seeded from
/// `template` `(md, body)` when available or synthesized standalone.
pub(super) fn checkpoint_step(
    template: Option<(&[u8], &[u8])>,
    summary: &str,
    after: &Reference<'_>,
    old_cascade: &str,
) -> Result<(Vec<u8>, Vec<u8>, u64, usize)> {
    if let Some((tpl_md, tpl_body)) = template {
        if let Some(cp_raw) = unique(tpl_body, 30, 2)? {
            let md = metadata(tpl_md, after)?;
            let rewritten = payload(tpl_body, after, old_cascade)?;
            let (clean_cp, saved, stripped) = checkpoint_prune(&cp_raw, Some(summary), 0)?;
            let mut body: Vec<u8> = Vec::new();
            walk(&rewritten, |f| {
                match f.number {
                    1 | 4 | 5 | 30 | 148 => {}
                    _ => body.extend(f.raw),
                }
                Ok(())
            })?;
            let mut header = Vec::new();
            scalar(&mut header, 1, 23);
            scalar(&mut header, 4, 3);
            bytes(&mut header, 5, &md);
            bytes(&mut header, 30, &clean_cp);
            header.extend(body);
            return Ok((md, header, saved, stripped));
        }
    }
    let mut md = Vec::new();
    scalar(&mut md, 3, 5);
    bytes(&mut md, 20, &after.encode(&[])?);
    let mut cp = Vec::new();
    scalar(&mut cp, 1, 1);
    bytes(&mut cp, 5, summary.as_bytes());
    scalar(&mut cp, 12, 0);
    scalar(&mut cp, 24, 1);
    scalar(&mut cp, 27, 0);
    let mut body = Vec::new();
    scalar(&mut body, 1, 23);
    scalar(&mut body, 4, 3);
    bytes(&mut body, 5, &md);
    bytes(&mut body, 30, &cp);
    Ok((md, body, 0, 0))
}

/// Rewrite an existing `step_type = 23` (`CHECKPOINT`) step body's `30.26` sludge sections.
pub(super) fn prune_existing_checkpoint_body(
    body: &[u8],
    replace_summary: Option<&str>,
    sludge_min_bytes: usize,
) -> Result<(Vec<u8>, u64, usize)> {
    let mut saved = 0u64;
    let mut stripped = 0usize;
    let rewritten = transform(body, |f| {
        if f.number == 30 {
            let (cp, s, n) = checkpoint_prune(f.value, replace_summary, sludge_min_bytes)?;
            saved += s;
            stripped += n;
            Ok(Some(cp))
        } else {
            Ok(None)
        }
    })?;
    Ok((rewritten, saved, stripped))
}

/// Omit `20.{3,4,5,14}` thinking & thought-signature fields on `PLANNER_RESPONSE` (`step_type = 15`).
pub(super) fn drop_planner_thinking(body: &[u8]) -> Result<(Vec<u8>, u64)> {
    let before = body.len();
    let rewritten = transform(body, |f| {
        if f.number != 20 {
            return Ok(None);
        }
        let mut inner = Vec::new();
        walk(f.value, |pf| {
            if !matches!(pf.number, 3 | 4 | 5 | 14) {
                inner.extend(pf.raw);
            }
            Ok(())
        })?;
        Ok(Some(inner))
    })?;
    Ok((rewritten.clone(), before.saturating_sub(rewritten.len()) as u64))
}

/// Snip oversized `140.2.1` tool outputs (plus `140.2.3` embedded `render_info` and `140.2.6`
/// binary `Any` payloads) on `CortexStepGeneric` (`step_type = 132`). Leaves `metadata.4`
/// `argumentsJson` and tool-call signatures completely untouched for Gemini API replay.
pub(super) fn snip_generic_step(
    md: &[u8],
    body: &[u8],
    step_idx: u64,
    min_size: usize,
    new_id: &str,
    drop: bool,
) -> Result<(Vec<u8>, Option<SnippedGeneric>, u64, bool)> {
    let Some(generic) = unique(body, 140, 2)? else {
        return Ok((body.to_vec(), None, 0, false));
    };
    let Some(result) = unique(&generic, 2, 2)? else {
        return Ok((body.to_vec(), None, 0, false));
    };
    let (opt_id, tool_name, input) = tool_call_meta(md, body, &generic)?;
    let call_id = opt_id.unwrap_or_else(|| format!("step_{step_idx}"));

    let mut output_bytes: Option<&[u8]> = None;
    let mut render_raw_len = 0usize;
    let mut any_raw_len = 0usize;
    walk(&result, |rf| {
        match (rf.number, rf.wire) {
            (1, 2) => output_bytes = Some(rf.value),
            (3, 2) => render_raw_len += rf.raw.len(),
            (6, 2) => any_raw_len += rf.raw.len(),
            _ => {}
        }
        Ok(())
    })?;

    let keep_invoke_any = matches!(tool_name.as_str(), "invoke_subagent" | "swarm");
    if let Some(out_slice) = output_bytes {
        if out_slice.len() > min_size && !out_slice.starts_with(crate::prune::MARKER_PREFIX.as_bytes()) {
            let original = String::from_utf8_lossy(out_slice).into_owned();
            let marker = crate::prune::build_marker(
                &call_id,
                &tool_name,
                &input,
                "text",
                original.len(),
                original.lines().count(),
                new_id,
                drop,
            );
            let mut new_res = Vec::new();
            let mut wrote_out = false;
            walk(&result, |rf| {
                match rf.number {
                    1 => {
                        if !wrote_out {
                            bytes(&mut new_res, 1, marker.as_bytes());
                            wrote_out = true;
                        }
                    }
                    // Clear embedded render_info and duplicate structured mirrors so UI/projection
                    // reads the concise marker rather than re-inflating the snipped body.
                    3 | 10 | 11 => {}
                    6 if !keep_invoke_any => {}
                    _ => new_res.extend(rf.raw),
                }
                Ok(())
            })?;
            let new_generic = transform(&generic, |gf| {
                if gf.number == 2 {
                    Ok(Some(new_res.clone()))
                } else {
                    Ok(None)
                }
            })?;
            let new_body = transform(body, |bf| {
                if bf.number == 140 {
                    Ok(Some(new_generic.clone()))
                } else {
                    Ok(None)
                }
            })?;
            let saved = body.len().saturating_sub(new_body.len()) as u64;
            return Ok((
                new_body,
                Some(SnippedGeneric {
                    call_id,
                    tool_name,
                    input,
                    original,
                    bytes_saved: saved,
                }),
                0,
                true,
            ));
        }

        // Even when `140.2.1` text fits within `min_size`, strip oversized duplicate UI `render_info`
        // (`140.2.3`) or `Any` (`140.2.6`) blobs when `140.2.1` is non-empty.
        let strip_render = !out_slice.is_empty() && render_raw_len > min_size;
        let strip_any = !out_slice.is_empty() && !keep_invoke_any && any_raw_len > min_size;
        if strip_render || strip_any {
            let mut new_res = Vec::new();
            walk(&result, |rf| {
                if (rf.number == 3 && strip_render) || (rf.number == 6 && strip_any) {
                    return Ok(());
                }
                new_res.extend(rf.raw);
                Ok(())
            })?;
            let new_generic = transform(&generic, |gf| {
                if gf.number == 2 {
                    Ok(Some(new_res.clone()))
                } else {
                    Ok(None)
                }
            })?;
            let new_body = transform(body, |bf| {
                if bf.number == 140 {
                    Ok(Some(new_generic.clone()))
                } else {
                    Ok(None)
                }
            })?;
            let saved = body.len().saturating_sub(new_body.len()) as u64;
            return Ok((new_body, None, saved, strip_render));
        }
    }

    Ok((body.to_vec(), None, 0, false))
}
