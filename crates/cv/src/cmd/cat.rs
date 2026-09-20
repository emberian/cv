//! `cv cat <session> <tool_use_id>` — one tool call's full output, wherever it lives.
//!
//! A tool result has three homes: inline in the transcript (the common case), in a `cv prune`
//! sidecar (`<stem>.flat.jsonl`, when prune snipped it and left a `[PRUNED id=…]` marker), or in a
//! persisted-output file (Claude Code writes oversized outputs to `<session>/tool-results/<id>.txt`
//! and leaves a stub the model saw; the adapter records the path in `details.persistedOutput.path`).
//! `cat` checks all three so the caller never has to know which. `--input` prints the matching
//! tool call's arguments instead. An unknown id is a usage error (exit 2).

use crate::util::{parse_harness, resolve, short_id, usage};
use anyhow::{Context, Result};
use cv_core::ir::Block;
use cv_core::ParseOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Prune's marker prefix (`cv_core::prune::MARKER_PREFIX` is private; the format is the sidecar's
/// public contract — retrieval keys on the id after it).
const PRUNED_MARKER: &str = "[PRUNED id=";

pub(crate) fn cmd_cat(session: &str, tool_use_id: &str, harness: Option<String>, input: bool) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = resolve(session, want)?;
    // Lazy parse: giant results stay on disk until the one block we print is resolved.
    let parsed = cv_core::stream::collect_with(adapter.as_ref(), &r, &ParseOptions::lazy())?;
    let resolver = parsed.resolver();
    let sidecar = sidecar_path(&r.path);
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());

    if input {
        for m in &parsed.messages {
            for b in &m.content {
                if let Block::ToolUse { id, name, input, .. } = b {
                    if id == tool_use_id {
                        eprintln!("✦ {name} ({id}) — input");
                        writeln!(out, "{}", serde_json::to_string_pretty(input)?)?;
                        return Ok(());
                    }
                }
            }
        }
        return usage(format!(
            "no tool call {tool_use_id:?} in {} — `cv tools {} --timeline` lists its tool calls",
            short_id(&r.id),
            short_id(&r.id)
        ));
    }

    for m in &parsed.messages {
        for b in &m.content {
            let Block::ToolResult {
                tool_use_id: tid,
                content,
                details,
                tool_name,
                ..
            } = b
            else {
                continue;
            };
            if tid != tool_use_id {
                continue;
            }
            // 1. A persisted-output file: the transcript holds only the stub the model saw.
            if let Some(p) = details
                .as_ref()
                .and_then(|d| d.pointer("/persistedOutput/path"))
                .and_then(|v| v.as_str())
            {
                let path = Path::new(p);
                if path.is_file() {
                    eprintln!("✦ persisted output: {p}");
                    let mut f = std::fs::File::open(path).with_context(|| format!("opening {p}"))?;
                    std::io::copy(&mut f, &mut out)?;
                    out.flush()?;
                    return Ok(());
                }
                eprintln!("⚠ persisted output {p} is missing — printing the transcript's stub instead");
            }
            let text = content.resolve(&resolver);
            // 2. A prune marker: the original lives in the sidecar next to the transcript.
            if text.trim_start().starts_with(PRUNED_MARKER) {
                if sidecar.is_file() {
                    if let Ok(v) = cv_core::prune::retrieve(&sidecar, tool_use_id) {
                        eprintln!("✦ from prune sidecar {}", sidecar.display());
                        return print_value(&mut out, &v);
                    }
                }
                eprintln!(
                    "⚠ {} was pruned but no sidecar holds it (hard-dropped with `--drop`?) — printing the marker",
                    tool_use_id
                );
            }
            // 3. Inline.
            if let Some(name) = tool_name {
                eprintln!("✦ {name} ({tool_use_id})");
            }
            writeln!(out, "{text}")?;
            out.flush()?;
            return Ok(());
        }
    }

    // Not in the transcript at all: a sidecar can still carry it (e.g. a `toolUseResult`-only
    // stash from a prune of a session whose result block was itself dropped).
    if sidecar.is_file() {
        if let Ok(v) = cv_core::prune::retrieve(&sidecar, tool_use_id) {
            eprintln!("✦ from prune sidecar {}", sidecar.display());
            return print_value(&mut out, &v);
        }
    }
    usage(format!(
        "no tool result {tool_use_id:?} in {} — `cv tools {} --timeline` lists its tool calls",
        short_id(&r.id),
        short_id(&r.id)
    ))
}

/// `<stem>.flat.jsonl` next to the transcript — where `cv prune` stashes snipped payloads.
///
/// `Path::parent` of a bare filename is `Some("")`, not `None`, so the empty parent has to be
/// filtered out explicitly or `abc.jsonl` would yield a bare `abc.flat.jsonl` sibling-of-nothing.
fn sidecar_path(transcript: &Path) -> PathBuf {
    let stem = transcript.file_stem().and_then(|s| s.to_str()).unwrap_or("session");
    transcript
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join(format!("{stem}.flat.jsonl"))
}

/// Raw text verbatim; structured payloads (image blocks, `toolUseResult` mirrors) pretty-printed.
fn print_value(out: &mut impl Write, v: &serde_json::Value) -> Result<()> {
    match v.as_str() {
        Some(s) => writeln!(out, "{s}")?,
        None => writeln!(out, "{}", serde_json::to_string_pretty(v)?)?,
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_sits_next_to_the_transcript() {
        assert_eq!(
            sidecar_path(Path::new("/p/-work/abc.jsonl")),
            PathBuf::from("/p/-work/abc.flat.jsonl")
        );
        // A bare filename resolves relative to `.`.
        assert_eq!(sidecar_path(Path::new("abc.jsonl")), PathBuf::from("./abc.flat.jsonl"));
    }
}
