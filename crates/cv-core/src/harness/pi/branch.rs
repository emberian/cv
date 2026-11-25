//! A selection of a Pi session written back natively, for `cv rewind` and `cv prune`.
//!
//! Pi's file is a tree of entries and its model context is the `parentId` chain from the leaf. A
//! selection (a prefix, or the whole session with tool outputs replaced by prune markers) is
//! therefore one branch: the header, then every entry on the chain from the root to the
//! selection's last entry, in file order. Each entry's bytes are copied as they are, so
//! compactions, `context_edit`s, model changes and entry kinds this reader does not know survive;
//! only the header (new `id`, `parentSession` naming the source file) and a pruned `toolResult`'s
//! text are rewritten. The file is read back with the Pi reader before it counts as written.

use crate::ir::{Block, Session};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Write `selected` (read from the Pi file `source`) as session `new_id` under `out_dir`. Returns
/// the written path and what the read-back did not reproduce.
pub fn write(source: &Path, selected: &Session, out_dir: &Path, new_id: &str) -> Result<(PathBuf, Vec<String>)> {
    let text = std::fs::read_to_string(source).with_context(|| format!("reading {}", source.display()))?;
    let bytes = branch(&text, &source.to_string_lossy(), selected, new_id)?;
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let path = out_dir.join(file_name(source, &selected.id, new_id));
    crate::publish::publish_reader(&path, "pi", bytes.as_bytes(), crate::publish::Claim::New)?;
    let back = super::parse_str(new_id, &bytes)?;
    Ok((path, verify(selected, &back, new_id)))
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
    let mut out = String::with_capacity(text.len());
    let mut head = entries[header].1.clone();
    head["id"] = json!(new_id);
    head["parentSession"] = json!(source_name);
    out.push_str(&serde_json::to_string(&head)?);
    out.push('\n');
    for (raw, v) in &entries[header + 1..] {
        if !id_of(v).is_some_and(|id| chain.contains(&id)) {
            continue;
        }
        let message = v.get("message");
        let call = message
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("toolResult"))
            .and_then(|m| m.get("toolCallId"))
            .and_then(Value::as_str);
        match call.and_then(|c| markers.get(c)) {
            Some(marker) => {
                let mut v = v.clone();
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
                out.push_str(&serde_json::to_string(&v)?);
            }
            None => out.push_str(raw),
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

    // #tooth A rewound Pi prefix is the root→cut branch, bytes copied: the abandoned sibling
    // leaves, an unknown entry kind on the branch survives, and it reads back entry for entry.
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
        let back = super::super::parse_str("n1", &out)?;
        assert_eq!(verify(&selected, &back, "n1"), Vec::<String>::new());
        Ok(())
    }

    // #tooth A pruned Pi session replaces only the old result's text with the marker; every
    // other entry is byte-identical and the marker reads back as the result.
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
        for line in src
            .lines()
            .skip(1)
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

    #[test]
    fn branch_files_keep_pis_created_prefix() {
        let p = Path::new("/s/--work--/2026-11-19T10-00-00-000Z_s1.jsonl");
        assert_eq!(file_name(p, "s1", "n1"), "2026-11-19T10-00-00-000Z_n1.jsonl");
        assert_eq!(file_name(Path::new("/s/other.jsonl"), "s1", "n1"), "n1.jsonl");
    }
}
