//! `cv cat <session> <tool_use_id>` — one tool call's full output, wherever it lives.
//!
//! A tool result has three homes: inline in the transcript (the common case), in a `cv prune`
//! sidecar (`<stem>.flat.jsonl`, when prune snipped it and left a `[PRUNED id=…]` marker), or in a
//! persisted-output file (Claude Code writes oversized outputs to `<session>/tool-results/<id>.txt`
//! and leaves a stub the model saw; the adapter records the path in `details.persistedOutput.path`).
//! `cat` checks all three so the caller never has to know which. `--input` prints the matching
//! tool call's arguments instead. An unknown id is a usage error (exit 2).

use crate::util::{parse_harness, short_id, usage};
use anyhow::{Context, Result};
use cv_core::ir::Block;
use cv_core::Demand;
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) fn cmd_cat(session: &str, tool_use_id: &str, harness: Option<String>, input: bool) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = super::distill::resolve_source(session, want)?;
    // Lazy stream to the first message holding the block, then stop: giant results stay on disk
    // until the one block we print is resolved, and nothing before or after it is kept.
    let mut found = First {
        id: tool_use_id,
        input,
        hit: None,
    };
    let mut parsed = cv_core::stream::stream_counted(
        adapter.as_ref(),
        &r,
        &Demand::LEAN.with_spans(),
        &mut found,
        cv_core::stream::Pass::RECORD,
    )?
    .session;
    parsed.messages = found.hit.into_iter().collect();
    let anchor = cv_core::harness::claude::artifacts::anchor(&r.path, &r.id);
    let mut sidecar = sidecar_path(&anchor);
    if !sidecar.try_exists()? {
        // A session pruned by IR selection keeps its sidecar as `<id>.flat.jsonl` beside it.
        // A path names the file, so its reference id can be the file stem; the sidecar is named
        // by the session id the transcript records.
        for id in [r.id.as_str(), parsed.id.as_str()] {
            let by_id = crate::cmd::reshape::ir_sidecar_path(&r.path, id);
            if by_id.try_exists()? {
                sidecar = by_id;
                break;
            }
        }
    }
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let bundle = cv_core::bundle::is_bundle(&r.path)
        .then(|| cv_core::bundle::Bundle::open(&r.path))
        .transpose()?;

    if input {
        for m in &parsed.messages {
            for b in &m.content {
                if let Block::ToolUse { id, name, input, .. } = b {
                    if id == tool_use_id {
                        eprintln!("✦ {name} ({id}) — input");
                        if matches!(name.as_str(), "Edit" | "Write") {
                            let marker = input.with_view(|v| {
                                let Some(value) = v.get("_cv_pruned_input")? else {
                                    return Ok(None);
                                };
                                Ok(value.resolve()?.as_str().map(str::to_owned))
                            })?;
                            match marker.as_deref() {
                                Some("dropped") => return usage(format!("input for {id:?} was removed with --drop")),
                                Some(cmd) if cmd.starts_with("cv cat ") => {
                                    if let Some(bundle) = &bundle {
                                        if print_bundled_input(bundle, id, &mut out)? {
                                            return Ok(());
                                        }
                                    }
                                    return print_stashed_input(&sidecar, id, &mut out);
                                }
                                _ => {}
                            }
                        }
                        writeln!(out, "{}", serde_json::to_string_pretty(input)?)?;
                        return Ok(());
                    }
                }
            }
        }
        if let Some(bundle) = &bundle {
            if print_bundled_input(bundle, tool_use_id, &mut out)? {
                return Ok(());
            }
        }
        if sidecar.try_exists()? {
            return print_stashed_input(&sidecar, tool_use_id, &mut out);
        }
        return usage(format!(
            "no tool call {tool_use_id:?} in {} — `cv tools {} --timeline` lists its tool calls",
            short_id(&r.id),
            short_id(&r.id)
        ));
    }

    if let Some(bundle) = &bundle {
        if print_bundled(&parsed, bundle, tool_use_id, &mut out)? {
            return Ok(());
        }
    }
    print_result(&parsed, &r.id, tool_use_id, &sidecar, Some(&anchor), &mut out)
}

fn print_bundled_input(bundle: &cv_core::bundle::Bundle, id: &str, out: &mut impl Write) -> Result<bool> {
    let call_id = format!("{id}#call");
    let found = match bundle.stashed_slot(id, "input")? {
        Some(v) => Some((true, v)),
        None => match bundle.stashed_slot(&call_id, "input")? {
            Some(v) => Some((true, v)),
            None => bundle.stashed_slot(&call_id, "content")?.map(|v| (false, v)),
        },
    };
    let Some((is_input_slot, val)) = found else {
        return Ok(false);
    };
    match val.as_str() {
        Some(raw) => out.write_all(raw.as_bytes())?,
        None if is_input_slot => writeln!(out, "{}", serde_json::to_string_pretty(&val)?)?,
        None => writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(val.get("input").context("stashed call has no input")?)?
        )?,
    }
    out.flush()?;
    Ok(true)
}

/// A `.cvbundle` answers from its own records: a persisted output by the path the transcript
/// names, a pruned output from the bundled sidecar. False: the transcript's inline text serves.
fn print_bundled(
    parsed: &cv_core::Session,
    bundle: &cv_core::bundle::Bundle,
    tool_use_id: &str,
    out: &mut impl Write,
) -> Result<bool> {
    let resolver = parsed.resolver();
    let found = parsed.messages.iter().flat_map(|m| &m.content).find_map(|b| match b {
        Block::ToolResult {
            tool_use_id: tid,
            content,
            details,
            ..
        } if tid == tool_use_id => Some((content, details)),
        _ => None,
    });
    let Some((content, details)) = found else {
        return match bundle.stashed(tool_use_id)? {
            Some(v) => print_value(out, &v).map(|()| true),
            None => Ok(false),
        };
    };
    if let Some(p) = details
        .as_ref()
        .and_then(|d| d.pointer("/persistedOutput/path"))
        .and_then(|v| v.as_str())
    {
        if let Some(bytes) = bundle.file(p)? {
            eprintln!("✦ persisted output (bundled): {p}");
            out.write_all(&bytes)?;
            out.flush()?;
            return Ok(true);
        }
    }
    let text = content.try_resolve(&resolver).context("reading bundled tool result")?;
    if text.trim_start().starts_with(cv_core::prune::MARKER_PREFIX) {
        if let Some(v) = bundle.stashed(tool_use_id)? {
            eprintln!("✦ from the bundled sidecar");
            print_value(out, &v)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn print_stashed_input(sidecar: &Path, id: &str, out: &mut impl Write) -> Result<()> {
    let call = cv_core::prune::retrieve(sidecar, &format!("{id}#call"))?;
    match call.as_str() {
        Some(raw) => out.write_all(raw.as_bytes())?,
        None => writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(call.get("input").context("stashed call has no input")?)?
        )?,
    }
    out.flush()?;
    Ok(())
}

fn print_result(
    parsed: &cv_core::Session,
    session_id: &str,
    tool_use_id: &str,
    sidecar: &Path,
    anchor: Option<&Path>,
    mut out: &mut impl Write,
) -> Result<()> {
    let resolver = parsed.resolver();
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
            } else if let Some(claimed) = details
                .as_ref()
                .and_then(|d| d.pointer("/persistedOutput/unresolved"))
                .and_then(|v| v.as_str())
            {
                eprintln!(
                    "⚠ the transcript says the full output is at {claimed}, which is not a file in this session's store — printing the stub instead"
                );
            }
            let text = content
                .try_resolve(&resolver)
                .context("reading transcript tool result")?;
            if text.trim_start().starts_with("<persisted-output>") && print_captured(anchor, tool_use_id, &mut out)? {
                return Ok(());
            }
            // 2. A prune marker: the original lives in the sidecar next to the transcript.
            if text.trim_start().starts_with(cv_core::prune::MARKER_PREFIX) {
                if sidecar.try_exists()? {
                    let v = cv_core::prune::retrieve(sidecar, tool_use_id)
                        .with_context(|| format!("reading prune sidecar {}", sidecar.display()))?;
                    eprintln!("✦ from prune sidecar {}", sidecar.display());
                    return print_sidecar_result(&mut out, &v, anchor, tool_use_id);
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
    if sidecar.try_exists()? {
        let v = cv_core::prune::retrieve(sidecar, tool_use_id)
            .with_context(|| format!("reading prune sidecar {}", sidecar.display()))?;
        eprintln!("✦ from prune sidecar {}", sidecar.display());
        return print_sidecar_result(&mut out, &v, anchor, tool_use_id);
    }
    usage(format!(
        "no tool result {tool_use_id:?} in {} — `cv tools {} --timeline` lists its tool calls",
        short_id(session_id),
        short_id(session_id)
    ))
}

fn print_captured(anchor: Option<&Path>, id: &str, out: &mut impl Write) -> Result<bool> {
    let Some(anchor) = anchor else { return Ok(false) };
    let Some(path) = cv_core::harness::claude::artifacts::captured_output(anchor, id)? else {
        return Ok(false);
    };
    std::io::copy(&mut std::fs::File::open(path)?, out)?;
    out.flush()?;
    Ok(true)
}

fn print_sidecar_result(
    out: &mut impl Write,
    value: &serde_json::Value,
    anchor: Option<&Path>,
    id: &str,
) -> Result<()> {
    let receipt = |v: &serde_json::Value| {
        v.as_str()
            .is_some_and(|s| s.trim_start().starts_with("<persisted-output>"))
    };
    if (receipt(value)
        || value
            .as_array()
            .is_some_and(|items| items.iter().any(|item| receipt(&item["text"]))))
        && print_captured(anchor, id, out)?
    {
        return Ok(());
    }
    print_value(out, value)
}

/// Keeps the first message carrying the wanted block — the `ToolUse` with that id under
/// `--input`, else the `ToolResult` for it — and stops the stream there.
struct First<'a> {
    id: &'a str,
    input: bool,
    hit: Option<cv_core::Message>,
}

impl cv_core::MessageSink for First<'_> {
    fn message(&mut self, m: cv_core::Message) -> cv_core::Flow {
        let wanted = m.content.iter().any(|b| match b {
            Block::ToolUse { id, .. } => self.input && id == self.id,
            Block::ToolResult { tool_use_id, .. } => !self.input && tool_use_id == self.id,
            _ => false,
        });
        if !wanted {
            return cv_core::Flow::Continue;
        }
        self.hit = Some(m);
        cv_core::Flow::Stop
    }
}

/// `<stem>.flat.jsonl` next to the transcript — where `cv prune` stashes snipped payloads.
///
/// `Path::parent` of a bare filename is `Some("")`, not `None`, so the empty parent has to be
/// filtered out explicitly or `abc.jsonl` would yield a bare `abc.flat.jsonl` sibling-of-nothing.
pub(crate) fn sidecar_path(transcript: &Path) -> PathBuf {
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
    fn corrupt_sidecar_is_an_error_with_its_path_not_a_drop_hint() {
        let world = crate::test_world("cat-corrupt-sidecar");
        let sidecar = world.base.join("s.flat.jsonl");
        std::fs::write(&sidecar, [0xff, b'\n']).unwrap();
        let session = cv_core::Session::new("s", cv_core::Harness::Claude);
        let mut out = Vec::new();
        let error = print_result(&session, "s", "tool", &sidecar, None, &mut out).unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("s.flat.jsonl"), "{error}");
        assert!(!error.contains("--drop"));
        assert!(out.is_empty());
    }

    #[test]
    fn retained_result_prints_exact_text_and_rejects_source_failure_without_output() {
        use cv_core::{Block, Flow, Harness, Message, MessageSink, Role, Session};
        let world = crate::test_world("cat-retained-result");
        let path = world.base.join("result.json");
        let body = "actual🌙\n\"\\".repeat(1024);
        let wire = serde_json::to_vec(&body).unwrap();
        std::fs::write(&path, &wire).unwrap();
        let content = cv_core::Json::source(&path, 0, wire.len() as u64)
            .unwrap()
            .with_view(|view| view.source_text())
            .unwrap()
            .unwrap();
        assert!(matches!(content, cv_core::Text::Source(_)));
        let mut message = Message::new(Role::Tool);
        message.content.push(Block::ToolResult {
            tool_use_id: "call".into(),
            content,
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
            extensions: Default::default(),
        });
        let mut first = First {
            id: "call",
            input: false,
            hit: None,
        };
        assert_eq!(first.message(message), Flow::Stop);
        let mut session = Session::new("cat", Harness::Codex);
        session.messages = first.hit.into_iter().collect();
        let sidecar = world.base.join("absent.flat.jsonl");
        let mut output = Vec::new();
        print_result(&session, &session.id, "call", &sidecar, None, &mut output).unwrap();
        assert_eq!(output, format!("{body}\n").as_bytes());
        let mut owned = session.clone();
        let resolver = owned.resolver();
        owned.messages[0].materialize(&resolver).unwrap();
        let mut expected = Vec::new();
        print_result(&owned, &owned.id, "call", &sidecar, None, &mut expected).unwrap();
        assert_eq!(output, expected);
        std::fs::remove_file(path).unwrap();
        let mut rejected = Vec::new();
        assert!(print_result(&session, &session.id, "call", &sidecar, None, &mut rejected).is_err());
        assert!(rejected.is_empty());
    }

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
