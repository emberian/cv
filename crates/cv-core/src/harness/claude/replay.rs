//! Portable Claude replay projects final wire fields, including raw carrier/extra overrides:
//! plaintext thinking becomes text, reasoning without plaintext is removed (a record left empty
//! goes too, its children relinked to its parent), and illegal tool IDs are remapped.
use crate::harness::store::{
    bytes::{ByteSource, ByteStage},
    document::range::{Range, RangeSource},
};
use anyhow::Result;
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, Write},
    sync::Arc,
};

#[derive(Default, Debug)]
pub(crate) struct Changes {
    pub ids: usize,
    /// Plaintext thinking blocks demoted to text.
    pub thinking: usize,
    /// Signature-only and redacted thinking blocks removed.
    pub dropped: usize,
    /// Records removed because every block in them was removed; their children are relinked.
    pub records: usize,
}
fn legal(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}
fn string(source: &RangeSource, range: Option<Range>) -> Result<Option<String>> {
    match range {
        Some(range) if source.kind(range)? == Some(b'"') => Ok(Some(source.materialize(range)?)),
        _ => Ok(None),
    }
}
fn blocks(source: &RangeSource, row: Range, mut visit: impl FnMut(Range) -> Result<()>) -> Result<()> {
    let mut cursor = source.cursor(row)?;
    cursor.whitespace()?;
    if cursor.position() == row.offset + row.len {
        return Ok(());
    }
    let Some(message) = source.get(row, "message")? else {
        return Ok(());
    };
    let Some(content) = source.get(message, "content")? else {
        return Ok(());
    };
    if let Some(array) = source.array(content)? {
        for block in array {
            visit(block?)?;
        }
    }
    Ok(())
}
/// Bounded framing for both memory and private spills; never retain an entire JSONL line.
pub(super) fn rows(source: &ByteSource, mut visit: impl FnMut(Range) -> Result<()>) -> Result<()> {
    let mut reader = BufReader::with_capacity(64 * 1024, source.reader()?);
    let (mut start, mut at) = (0u64, 0u64);
    loop {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            if at > start {
                visit(Range {
                    offset: start,
                    len: at - start,
                })?;
            }
            break;
        }
        let n = bytes.len();
        let mut position = 0;
        while let Some(delta) = memchr::memchr(b'\n', &bytes[position..]) {
            position += delta + 1;
            let end = at + position as u64;
            visit(Range {
                offset: start,
                len: end - start,
            })?;
            start = end;
        }
        at += n as u64;
        reader.consume(n);
    }
    Ok(())
}
pub(super) fn copy(source: &RangeSource, range: Range, output: &mut ByteStage) -> Result<()> {
    std::io::copy(&mut source.cursor(range)?, output)?;
    Ok(())
}
enum Replacement {
    Bytes(Vec<u8>),
    Thinking(Range),
    Remove,
}
/// Portable fate of one content block: plaintext thinking becomes text; reasoning with no
/// plaintext (signature-only, redacted) has nothing to carry and is removed.
enum Fate {
    Keep,
    Text(Range),
    Drop,
}
fn fate(source: &RangeSource, block: Range, ty: Option<&str>, mode: crate::emit::ThinkingMode) -> Result<Fate> {
    if mode != crate::emit::ThinkingMode::Portable {
        return Ok(Fate::Keep);
    }
    Ok(match ty {
        Some("thinking") => match source.get(block, "thinking")? {
            Some(range) if source.kind(range)? == Some(b'"') && range.len > 2 => Fate::Text(range),
            _ => Fate::Drop,
        },
        Some("redacted_thinking") => Fate::Drop,
        _ => Fate::Keep,
    })
}
/// Record links that may name a removed record; each is redirected to its nearest kept ancestor.
const LINKS: [&str; 3] = ["parentUuid", "logicalParentUuid", "leafUuid"];
fn surviving<'a>(removed: &'a HashMap<String, Option<String>>, mut id: &'a str) -> Option<&'a str> {
    for _ in 0..=removed.len() {
        match removed.get(id) {
            None => return Some(id),
            Some(Some(parent)) => id = parent,
            Some(None) => return None,
        }
    }
    None
}

/// Two passes reserve all valid IDs (and find records left empty) before allocation. Metadata
/// scales with IDs, removed records and each record's selected spans. Unknown fields and tool
/// payloads copy unchanged with bounded positioned reads.
pub(crate) fn normalize(bytes: ByteSource, mode: crate::emit::ThinkingMode) -> Result<(ByteStage, Changes)> {
    rewrite(bytes, mode, false)
}

/// Remove records whose `message.content` is an empty array (a transform emptied them), relinking
/// their children exactly as [`normalize`] does. Nothing else changes.
pub(crate) fn drop_empty_records(bytes: ByteSource) -> Result<(ByteStage, Changes)> {
    rewrite(bytes, crate::emit::ThinkingMode::Native, true)
}

fn empty_content(source: &RangeSource, row: Range) -> Result<bool> {
    let Some(message) = source.get(row, "message")? else {
        return Ok(false);
    };
    let Some(content) = source.get(message, "content")? else {
        return Ok(false);
    };
    Ok(match source.array(content)? {
        Some(mut array) => array.next().is_none(),
        None => false,
    })
}

fn rewrite(bytes: ByteSource, mode: crate::emit::ThinkingMode, empties: bool) -> Result<(ByteStage, Changes)> {
    let bytes = Arc::new(bytes);
    let source = RangeSource::private(bytes.clone())?;
    let mut reserved = HashSet::new();
    let mut invalid = Vec::new();
    let mut seen = HashSet::new();
    // Claude Code writes one content block per record, so a signature-only block usually empties
    // its record. Such a record is removed and its children point at its parent instead.
    let mut removed: HashMap<String, Option<String>> = HashMap::new();
    let mut removed_rows = HashSet::new();
    rows(&bytes, |row| {
        // A portable output must not silently carry invalid JSON around its repaired fields.
        let mut cursor = source.cursor(row)?;
        cursor.whitespace()?;
        if cursor.position() == row.offset + row.len {
            return Ok(());
        }
        if !source.validate(row)? {
            // A transform's pass-through keeps malformed lines verbatim; replay refuses them.
            anyhow::ensure!(empties, "portable Claude replay encountered invalid JSON");
            return Ok(());
        }
        let (mut total, mut drops) = (0usize, 0usize);
        blocks(&source, row, |block| {
            let ty = string(&source, source.get(block, "type")?)?;
            total += 1;
            if matches!(fate(&source, block, ty.as_deref(), mode)?, Fate::Drop) {
                drops += 1;
            }
            let key = match ty.as_deref() {
                _ if empties => return Ok(()),
                Some("tool_use") => "id",
                Some("tool_result") => "tool_use_id",
                _ => return Ok(()),
            };
            let id = string(&source, source.get(block, key)?)?
                .ok_or_else(|| anyhow::anyhow!("portable Claude replay: {key} must be a string"))?;
            if legal(&id) {
                reserved.insert(id);
            } else if seen.insert(id.clone()) {
                invalid.push(id);
            }
            Ok(())
        })?;
        if (total > 0 && drops == total) || (empties && total == 0 && empty_content(&source, row)?) {
            removed_rows.insert(row.offset);
            if let Some(uuid) = string(&source, source.get(row, "uuid")?)? {
                removed.insert(uuid, string(&source, source.get(row, "parentUuid")?)?);
            }
        }
        Ok(())
    })?;
    let mut mapping = HashMap::new();
    let mut next = 0usize;
    for original in invalid {
        loop {
            let replacement = format!("cv_tool_{next}");
            next += 1;
            if reserved.insert(replacement.clone()) {
                mapping.insert(original, serde_json::to_vec(&replacement)?);
                break;
            }
        }
    }
    let mut output = ByteStage::new();
    let mut changes = Changes {
        ids: mapping.len(),
        ..Changes::default()
    };
    rows(&bytes, |row| {
        if empties && !removed_rows.contains(&row.offset) && (removed.is_empty() || !source.validate(row)?) {
            return copy(&source, row, &mut output);
        }
        let mut patches = Vec::new();
        // (block, kept) for every content element, in order.
        let mut elements: Vec<(Range, bool)> = Vec::new();
        blocks(&source, row, |block| {
            let ty = string(&source, source.get(block, "type")?)?;
            let fate = fate(&source, block, ty.as_deref(), mode)?;
            elements.push((block, !matches!(fate, Fate::Drop)));
            match (ty.as_deref(), fate) {
                (_, Fate::Text(text)) => {
                    changes.thinking += 1;
                    patches.push((block, Replacement::Thinking(text)));
                }
                (Some(ty @ ("tool_use" | "tool_result")), _) => {
                    let range = source.get(block, if ty == "tool_use" { "id" } else { "tool_use_id" })?;
                    if let Some(id) = string(&source, range)? {
                        if let Some(replacement) = mapping.get(&id) {
                            patches.push((range.unwrap(), Replacement::Bytes(replacement.clone())));
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        })?;
        let drops = elements.iter().filter(|e| !e.1).count();
        changes.dropped += drops;
        if removed_rows.contains(&row.offset) {
            changes.records += 1;
            return Ok(());
        }
        if !removed.is_empty() {
            for key in LINKS {
                let range = source.get(row, key)?;
                if let Some(link) = string(&source, range)? {
                    if removed.contains_key(&link) {
                        let target = surviving(&removed, &link);
                        patches.push((range.unwrap(), Replacement::Bytes(serde_json::to_vec(&target)?)));
                    }
                }
            }
        }
        // A removed element takes the separator after it; a trailing run takes the one before it.
        let last_kept = elements.iter().rposition(|e| e.1);
        for (i, (block, kept)) in elements.iter().enumerate() {
            if *kept {
                continue;
            }
            match last_kept {
                Some(k) if i < k => {
                    let len = elements[i + 1].0.offset - block.offset;
                    patches.push((
                        Range {
                            offset: block.offset,
                            len,
                        },
                        Replacement::Remove,
                    ));
                }
                _ => {
                    let from = last_kept.map_or(block.offset, |k| elements[k].0.offset + elements[k].0.len);
                    let last = elements.last().unwrap().0;
                    patches.push((
                        Range {
                            offset: from,
                            len: last.offset + last.len - from,
                        },
                        Replacement::Remove,
                    ));
                    break;
                }
            }
        }
        patches.sort_by_key(|p| p.0.offset);
        let mut position = row.offset;
        for (range, replacement) in patches {
            copy(
                &source,
                Range {
                    offset: position,
                    len: range.offset - position,
                },
                &mut output,
            )?;
            match replacement {
                Replacement::Bytes(bytes) => output.write_all(&bytes)?,
                Replacement::Thinking(text) => {
                    output.write_all(b"{\"type\":\"text\",\"text\":")?;
                    copy(&source, text, &mut output)?;
                    output.write_all(b"}")?;
                }
                Replacement::Remove => {}
            }
            position = range.offset + range.len;
        }
        copy(
            &source,
            Range {
                offset: position,
                len: row.offset + row.len - position,
            },
            &mut output,
        )
    })?;
    Ok((output, changes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn portable_preserves_payloads_and_maps_collisions_without_model_guesses() -> Result<()> {
        let body = "giant \\ \" 🌙\n".repeat(100_000);
        let calls = json!({"type":"assistant","message":{"role":"assistant","model":"claude-alias","content":[
            {"type":"thinking","thinking":body,"signature":"foreign"},
            {"type":"thinking","thinking":"","signature":"opaque"},
            {"type":"redacted_thinking","data":"opaque","thinking":"do not interpret this"},
            {"type":"tool_use","id":"a:b","name":"Read","input":{"body":body}},
            {"type":"tool_use","id":"a#b","name":"Read","input":{}},
            {"type":"tool_use","id":"cv_tool_0","name":"Read","input":{}},
            {"type":"tool_use","id":"","name":"Read","input":{}},
            {"type":"tool_use","id":"☃","name":"Read","input":{}}
        ]},"unknown":{"keep":"a:b"}});
        let results = json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"a:b","content":body},
            {"type":"tool_result","tool_use_id":"a#b","content":"ok"},
            {"type":"tool_result","tool_use_id":"cv_tool_0","content":"ok"},
            {"type":"tool_result","tool_use_id":"","content":"ok"},
            {"type":"tool_result","tool_use_id":"☃","content":"ok"}
        ]}});
        let mut stage = ByteStage::new();
        // Results-before-calls must use the same mapping as ordinary call/result ordering.
        writeln!(stage, "{results}")?;
        writeln!(stage, "{calls}")?;
        let (stage, changes) = normalize(stage.finish()?, crate::emit::ThinkingMode::Portable)?;
        assert_eq!(
            (changes.ids, changes.thinking, changes.dropped, changes.records),
            (4, 1, 2, 0)
        );
        let rows = parsed(stage)?;
        let content = rows[1]["message"]["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], body);
        assert_eq!(
            content.len(),
            6,
            "signature-only and redacted reasoning are removed, not replaced"
        );
        let ids: Vec<_> = content[1..].iter().map(|b| b["id"].as_str().unwrap()).collect();
        assert!(ids.iter().all(|id| legal(id)));
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 5);
        assert_eq!(ids[2], "cv_tool_0");
        for (result, id) in rows[0]["message"]["content"].as_array().unwrap().iter().zip(ids) {
            assert_eq!(result["tool_use_id"], id);
        }
        assert_eq!(content[1]["input"], calls["message"]["content"][3]["input"]);
        assert_eq!(rows[0]["message"]["content"][0]["content"], body);
        assert_eq!(rows[1]["unknown"], calls["unknown"]);
        Ok(())
    }
    fn parsed(stage: ByteStage) -> Result<Vec<serde_json::Value>> {
        let output = stage.finish()?;
        let rows = BufReader::new(output.reader()?)
            .lines()
            .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
            .collect();
        Ok(rows)
    }
    #[test]
    fn portable_removes_records_left_empty_and_relinks_their_children() -> Result<()> {
        let thinking = |uuid: &str, parent: &str| {
            json!({"type":"assistant","uuid":uuid,"parentUuid":parent,"message":{"role":"assistant",
                "content":[{"type":"thinking","thinking":"","signature":"opaque"}]}})
        };
        let mut stage = ByteStage::new();
        writeln!(
            stage,
            "{}",
            json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"go"}})
        )?;
        writeln!(stage, "{}", thinking("a1", "u1"))?;
        writeln!(
            stage,
            "{}",
            json!({"type":"assistant","uuid":"a2","parentUuid":"a1","message":{"content":[
            {"type":"text","text":"kept"},{"type":"redacted_thinking","data":"x"},{"type":"thinking","thinking":"","signature":"s"}]}})
        )?;
        writeln!(stage, "{}", thinking("a3", "a2"))?;
        writeln!(stage, "{}", thinking("a4", "a3"))?;
        writeln!(
            stage,
            "{}",
            json!({"type":"assistant","uuid":"a5","parentUuid":"a4","message":{"content":[
            {"type":"redacted_thinking","data":"x"},{"type":"tool_use","id":"t","name":"Read","input":{}}]}})
        )?;
        let (stage, changes) = normalize(stage.finish()?, crate::emit::ThinkingMode::Portable)?;
        assert_eq!((changes.thinking, changes.dropped, changes.records), (0, 6, 3));
        let rows = parsed(stage)?;
        let links: Vec<_> = rows
            .iter()
            .map(|r| (r["uuid"].as_str().unwrap(), r["parentUuid"].clone()))
            .collect();
        assert_eq!(links, [("u1", json!(null)), ("a2", json!("u1")), ("a5", json!("a2"))]);
        assert_eq!(rows[1]["message"]["content"], json!([{"type":"text","text":"kept"}]));
        assert_eq!(
            rows[2]["message"]["content"],
            json!([{"type":"tool_use","id":"t","name":"Read","input":{}}])
        );
        Ok(())
    }
    #[test]
    fn portable_rejects_missing_ids_and_invalid_json() -> Result<()> {
        for line in [
            r#"{"message":{"content":[{"type":"tool_use","name":"Read"}]}}"#,
            r#"{"message":{"content":[{"type":"tool_result","tool_use_id":null}]}}"#,
            r#"{"invalid":"\ud800"}"#,
        ] {
            let mut stage = ByteStage::new();
            stage.write_all(line.as_bytes())?;
            assert!(normalize(stage.finish()?, crate::emit::ThinkingMode::Portable).is_err());
        }
        Ok(())
    }
}
