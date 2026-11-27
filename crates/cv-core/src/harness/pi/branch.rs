//! A selection of a Pi session written back natively, for `cv rewind` and `cv prune`.
//!
//! Pi's file is a tree of entries and its model context is the `parentId` chain from the leaf. A
//! selection (a prefix, or the whole session with tool outputs replaced by prune markers) is
//! therefore one branch: the header, then every entry on the chain from the root to the
//! selection's last entry, in file order. Each entry's bytes are copied as they are unless its
//! `parentId` needs relinking (from the old `session` header id to `new_id`, or past a dropped
//! `context_edit` that targeted a pruned `toolResult`) or it is itself a pruned `toolResult`.
//! The file is read back with the Pi reader before it counts as written.

use crate::ir::{Block, Session};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Write `selected` (read from the Pi file `source`) as session `new_id` under `out_dir`. Returns
/// the written path and what the read-back did not reproduce.
pub fn write(source: &Path, selected: &Session, out_dir: &Path, new_id: &str) -> Result<(PathBuf, Vec<String>)> {
    let (path, _, lost) = write_with_sidecar(source, selected, out_dir, new_id, &[])?;
    Ok((path, lost))
}

/// [`write`] with optional elided sidecar entries staged and published atomically before the
/// reshaped Pi transcript.
pub fn write_with_sidecar(
    source: &Path,
    selected: &Session,
    out_dir: &Path,
    new_id: &str,
    sidecar: &[crate::distill::Elided],
) -> Result<(PathBuf, Option<PathBuf>, Vec<String>)> {
    crate::publish::validate_session_id(new_id)?;
    let text = std::fs::read_to_string(source).with_context(|| format!("reading {}", source.display()))?;
    let bytes = branch(&text, &source.to_string_lossy(), selected, new_id)?;
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let path = out_dir.join(file_name(source, &selected.id, new_id));
    let sidecar_path = (!sidecar.is_empty()).then(|| out_dir.join(format!("{new_id}.flat.jsonl")));
    let mut set = crate::publish::Set::new();
    if let Some(sc_path) = &sidecar_path {
        let staged_sc = crate::distill::stage_sidecar(sc_path, sidecar)?;
        set.add(staged_sc, sc_path, crate::publish::Claim::New);
    }
    set.stage(&path, "pi", bytes.as_bytes(), crate::publish::Claim::New)?;
    let back = super::parse_str(new_id, &bytes)?;
    let lost = verify(selected, &back, new_id);
    set.publish()?;
    Ok((path, sidecar_path, lost))
}

/// Pi names a file `<created>_<id>.jsonl`; the branch keeps the source's prefix under its own id.
fn file_name(source: &Path, source_id: &str, new_id: &str) -> String {
    let stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    match stem.strip_suffix(source_id).filter(|p| p.ends_with('_')) {
        Some(prefix) => format!("{prefix}{new_id}.jsonl"),
        None => format!("{new_id}.jsonl"),
    }
}

/// The branch's JSONL text: see the module docs.
fn branch(text: &str, source_name: &str, selected: &Session, new_id: &str) -> Result<String> {
    crate::publish::validate_session_id(new_id)?;
    // (raw line, parsed entry) for every line that is a JSON object; the reader skips the rest.
    let entries: Vec<(&str, Value)> = text
        .lines()
        .filter_map(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .filter(Value::is_object)
                .map(|v| (line, v))
        })
        .collect();
    let id_of = |v: &Value| v.get("id").and_then(Value::as_str).map(str::to_owned);
    let Some(header) = entries
        .iter()
        .position(|(_, v)| v.get("type").and_then(Value::as_str) == Some("session"))
    else {
        bail!("not a Pi session: no `session` header entry");
    };
    let old_header_id = id_of(&entries[header].1);
    let parent: HashMap<String, Option<String>> = entries
        .iter()
        .filter_map(|(_, v)| {
            let id = id_of(v)?;
            Some((id, v.get("parentId").and_then(Value::as_str).map(str::to_owned)))
        })
        .collect();
    let Some(leaf) = selected
        .messages
        .iter()
        .rev()
        .find_map(|m| m.id.as_deref().filter(|id| parent.contains_key(*id)))
    else {
        bail!("the selection names no entry of the Pi source");
    };
    let mut chain: HashSet<String> = HashSet::new();
    let mut at = Some(leaf.to_owned());
    while let Some(id) = at {
        if !chain.insert(id.clone()) {
            bail!("the Pi source's parentId chain loops at entry {id}");
        }
        at = parent.get(&id).cloned().flatten();
    }
    // Pruned tool outputs: tool_use_id → the marker that replaces the result's text.
    let resolver = selected.resolver();
    let mut markers: HashMap<&str, String> = HashMap::new();
    for m in &selected.messages {
        for b in &m.content {
            if let Block::ToolResult {
                tool_use_id, content, ..
            } = b
            {
                let t = content.try_resolve(&resolver)?;
                if t.starts_with(crate::prune::MARKER_PREFIX) {
                    markers.insert(tool_use_id.as_str(), t.into_owned());
                }
            }
        }
    }

    // Collect entry IDs of pruned `toolResult` rows so any later `context_edit` targeting one is
    // dropped (and its children relinked) instead of resurrecting unpruned output on readback.
    let mut pruned_result_ids: HashSet<String> = HashSet::new();
    for (_, v) in &entries[header + 1..] {
        let Some(eid) = id_of(v).filter(|id| chain.contains(id)) else {
            continue;
        };
        let call = v
            .get("message")
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("toolResult"))
            .and_then(|m| m.get("toolCallId"))
            .and_then(Value::as_str);
        if call.is_some_and(|c| markers.contains_key(c)) {
            pruned_result_ids.insert(eid);
        }
    }

    let mut parent_rewrites: HashMap<String, Option<String>> = HashMap::new();
    if let Some(old_id) = &old_header_id {
        if old_id != new_id {
            parent_rewrites.insert(old_id.clone(), Some(new_id.to_owned()));
        }
    }
    let mut dropped_edits: HashSet<String> = HashSet::new();
    for (_, v) in &entries[header + 1..] {
        let Some(eid) = id_of(v).filter(|id| chain.contains(id)) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) == Some("context_edit")
            && v.get("targetId")
                .and_then(Value::as_str)
                .is_some_and(|tid| pruned_result_ids.contains(tid))
        {
            let raw_parent = v.get("parentId").and_then(Value::as_str);
            let resolved_parent = match raw_parent {
                Some(p) => parent_rewrites.get(p).cloned().unwrap_or_else(|| Some(p.to_owned())),
                None => None,
            };
            dropped_edits.insert(eid.clone());
            parent_rewrites.insert(eid, resolved_parent);
        }
    }

    let mut out = String::with_capacity(text.len());
    let mut head = entries[header].1.clone();
    head["id"] = json!(new_id);
    head["parentSession"] = json!(source_name);
    out.push_str(&serde_json::to_string(&head)?);
    out.push('\n');
    for (raw, v) in &entries[header + 1..] {
        let Some(eid) = id_of(v).filter(|id| chain.contains(id)) else {
            continue;
        };
        if dropped_edits.contains(&eid) {
            continue;
        }
        let message = v.get("message");
        let call = message
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("toolResult"))
            .and_then(|m| m.get("toolCallId"))
            .and_then(Value::as_str);
        let rewritten_parent = v
            .get("parentId")
            .and_then(Value::as_str)
            .and_then(|p| parent_rewrites.get(p));
        match (call.and_then(|c| markers.get(c)), rewritten_parent) {
            (None, None) => out.push_str(raw),
            (marker_opt, parent_opt) => {
                let mut v = v.clone();
                if let Some(new_parent) = parent_opt {
                    v["parentId"] = match new_parent {
                        Some(p) => json!(p),
                        None => Value::Null,
                    };
                }
                if let Some(marker) = marker_opt {
                    let message = &mut v["message"];
                    // The text parts become the marker; images and other parts stay.
                    let kept: Vec<Value> = message
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter(|p| p.get("type").and_then(Value::as_str) != Some("text"))
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut content = vec![json!({"type": "text", "text": marker})];
                    content.extend(kept);
                    message["content"] = Value::Array(content);
                }
                out.push_str(&serde_json::to_string(&v)?);
            }
        }
        out.push('\n');
    }
    Ok(out)
}

/// What the read-back lost: the entries the selection names, in order, and its id.
fn verify(selected: &Session, back: &Session, new_id: &str) -> Vec<String> {
    let ids = |s: &Session| s.messages.iter().filter_map(|m| m.id.clone()).collect::<Vec<_>>();
    let mut lost = Vec::new();
    let (want, got) = (ids(selected), ids(back));
    if want != got {
        lost.push(format!("entries: {} selected → {} (read back)", want.len(), got.len()));
    }
    if back.id != new_id {
        lost.push(format!("id: {new_id} → {} (read back)", back.id));
    }
    lost
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_testkit::jsonl;

    fn source() -> String {
        jsonl(&[
            json!({"type":"session","version":3,"id":"s1","cwd":"/work","timestamp":"2026-11-19T10:00:00Z"}),
            json!({"type":"session_info","id":"si","parentId":"s1","name":"Run"}),
            json!({"type":"message","id":"u1","parentId":"si","message":{"role":"user","content":[{"type":"text","text":"read the log"}]}}),
            json!({"type":"message","id":"a1","parentId":"u1","message":{"role":"assistant","content":[{"type":"toolCall","id":"t1","name":"read","arguments":{"path":"/work/big.log"}}]}}),
            json!({"type":"message","id":"r1","parentId":"a1","message":{"role":"toolResult","toolCallId":"t1","toolName":"read","content":[{"type":"text","text":"x".repeat(4000)}],"isError":false}}),
            json!({"type":"message","id":"dead","parentId":"r1","message":{"role":"assistant","content":[{"type":"text","text":"abandoned"}]}}),
            json!({"type":"future_entry","id":"f1","parentId":"r1","payload":{"kept":true}}),
            json!({"type":"message","id":"a2","parentId":"f1","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}),
            json!({"type":"message","id":"u2","parentId":"a2","message":{"role":"user","content":[{"type":"text","text":"next"}]}}),
            json!({"type":"message","id":"a3","parentId":"u2","message":{"role":"assistant","content":[{"type":"text","text":"later"}]}}),
        ])
    }

    // #tooth A rewound Pi prefix is the root→cut branch: children of the old header relink to
    // `new_id`, abandoned siblings leave, unknown entry kinds survive, and it reads back cleanly.
    #[test]
    fn rewind_writes_the_branch_through_the_cut() -> Result<()> {
        let src = source();
        let mut selected = super::super::parse_str("s1", &src)?;
        let cut = selected
            .messages
            .iter()
            .position(|m| m.id.as_deref() == Some("a2"))
            .unwrap();
        selected.messages.truncate(cut + 1);
        let out = branch(&src, "/store/s1.jsonl", &selected, "n1")?;
        assert!(!out.contains("abandoned") && !out.contains("\"u2\""), "{out}");
        assert!(out.contains(r#"{"type":"future_entry","id":"f1","parentId":"r1","payload":{"kept":true}}"#));
        assert!(out.starts_with(r#"{"type":"session","version":3,"id":"n1","#), "{out}");
        assert!(out
            .lines()
            .next()
            .unwrap()
            .contains(r#""parentSession":"/store/s1.jsonl""#));
        let si: Value = serde_json::from_str(out.lines().nth(1).unwrap())?;
        assert_eq!(si["id"], "si");
        assert_eq!(si["parentId"], "n1", "child of old header must relink to new_id");
        let back = super::super::parse_str("n1", &out)?;
        assert_eq!(verify(&selected, &back, "n1"), Vec::<String>::new());
        Ok(())
    }

    // #tooth A pruned Pi session replaces the old result's text with the marker, relinks direct
    // children of the header to `new_id`, and leaves every other live entry byte-identical.
    #[test]
    fn prune_rewrites_only_the_pruned_result() -> Result<()> {
        let src = source();
        let selected = super::super::parse_str("s1", &src)?;
        let opts = crate::prune::PruneOptions {
            min_size: 1024,
            keep_last: 2,
            ..Default::default()
        };
        let pruned = crate::prune::prune_ir(selected, &opts, "n2")?;
        assert_eq!(pruned.pruned_count, 1);
        let out = branch(&src, "s1.jsonl", &pruned.session, "n2")?;
        assert!(!out.contains(&"x".repeat(4000)));
        assert!(!out.contains(r#""parentId":"s1""#), "{out}");
        for line in src
            .lines()
            .skip(2)
            .filter(|l| !l.contains("\"r1\"") && !l.contains("\"dead\""))
        {
            assert!(out.contains(line), "{line}");
        }
        let back = super::super::parse_str("n2", &out)?;
        assert_eq!(verify(&pruned.session, &back, "n2"), Vec::<String>::new());
        let resolver = back.resolver();
        let marker = back
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                Block::ToolResult { content, .. } => Some(content.try_resolve(&resolver).unwrap().into_owned()),
                _ => None,
            })
            .unwrap();
        assert!(marker.starts_with(crate::prune::MARKER_PREFIX), "{marker}");
        Ok(())
    }

    // #tooth A `context_edit` targeting a pruned `toolResult` is dropped and its descendants are
    // relinked so `apply_context_edits` cannot resurrect the unpruned payload on readback.
    #[test]
    fn prune_drops_context_edit_targeting_pruned_result_and_relinks_children() -> Result<()> {
        let secret = "UNPRUNED_SECRET_PAYLOAD_".repeat(120);
        let src = jsonl(&[
            json!({"type":"session","version":3,"id":"s1","cwd":"/work","timestamp":"2026-11-19T10:00:00Z"}),
            json!({"type":"message","id":"u1","parentId":"s1","message":{"role":"user","content":[{"type":"text","text":"go"}]}}),
            json!({"type":"message","id":"a1","parentId":"u1","message":{"role":"assistant","content":[{"type":"toolCall","id":"t1","name":"read","arguments":{"path":"/work/f"}}]}}),
            json!({"type":"message","id":"r1","parentId":"a1","message":{"role":"toolResult","toolCallId":"t1","toolName":"read","content":[{"type":"text","text":"initial"}],"isError":false}}),
            json!({"type":"context_edit","id":"ce1","parentId":"r1","targetId":"r1","replacement":{"content":[{"type":"text","text":secret}]}}),
            json!({"type":"message","id":"a2","parentId":"ce1","message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}}),
            json!({"type":"message","id":"u2","parentId":"a2","message":{"role":"user","content":[{"type":"text","text":"more"}]}}),
            json!({"type":"message","id":"a3","parentId":"u2","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}),
        ]);
        let selected = super::super::parse_str("s1", &src)?;
        let opts = crate::prune::PruneOptions {
            min_size: 512,
            keep_last: 2,
            ..Default::default()
        };
        let pruned = crate::prune::prune_ir(selected, &opts, "n3")?;
        assert_eq!(pruned.pruned_count, 1);
        let out = branch(&src, "s1.jsonl", &pruned.session, "n3")?;
        assert!(!out.contains("UNPRUNED_SECRET_PAYLOAD_"), "{out}");
        assert!(!out.contains("\"ce1\""), "{out}");
        let back = super::super::parse_str("n3", &out)?;
        assert_eq!(verify(&pruned.session, &back, "n3"), Vec::<String>::new());
        let resolver = back.resolver();
        let result_text = back
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                Block::ToolResult { content, .. } => Some(content.try_resolve(&resolver).unwrap().into_owned()),
                _ => None,
            })
            .unwrap();
        assert!(result_text.starts_with(crate::prune::MARKER_PREFIX), "{result_text}");
        assert!(!result_text.contains("UNPRUNED_SECRET_PAYLOAD_"));
        Ok(())
    }

    #[test]
    fn branch_files_keep_pis_created_prefix() {
        let p = Path::new("/s/--work--/2026-11-19T10-00-00-000Z_s1.jsonl");
        assert_eq!(file_name(p, "s1", "n1"), "2026-11-19T10-00-00-000Z_n1.jsonl");
        assert_eq!(file_name(Path::new("/s/other.jsonl"), "s1", "n1"), "n1.jsonl");
    }
}
