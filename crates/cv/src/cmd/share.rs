//! `cv share` — redact a session and emit one self-contained HTML artifact.
//!
//! The output is a single `.html` file anyone can open offline (`file://`, no install, no
//! upload): inline CSS/JS, sticky header, collapsible tool calls and thinking blocks, keyboard
//! nav, and a redaction badge. Rendering **streams** — each message is materialized, scrubbed,
//! rendered, written, and dropped, so a multi-GB transcript shares at O(largest message).
//!
//! Redaction (cv_core::redact) is ON by default; `--no-redact` skips it with a loud stderr
//! warning. The document pieces live in [`cv_core::html`] (`share_begin` / `share_message` /
//! `share_end`); this module owns the streaming sink and the CLI surface.

use super::frames::Frames;
use crate::util::{home_rel, parse_harness, resolve};
use anyhow::{Context, Result};
use cv_core::html::{self, ShareMeta};
use cv_core::ir::{human_bytes, label_from, Harness, Message, Session};
use cv_core::redact::{redact_message, redact_text, RedactOptions, RedactStats};
use cv_core::{Demand, Flow, MessageSink};
use std::io::Write;
use std::path::PathBuf;

pub(crate) fn cmd_share(
    id: &str,
    harness: Option<String>,
    out: Option<PathBuf>,
    force: bool,
    no_redact: bool,
) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = resolve(id, want)?;

    if no_redact {
        eprintln!("⚠ --no-redact: secrets and PII will NOT be scrubbed — review the file before sharing it");
    }

    let out_path = out.unwrap_or_else(|| PathBuf::from(format!("{}.html", safe_stem(&r.id))));
    let claim = crate::util::user_file_claim(&out_path, force)?;
    let (count, stats) = share_session(adapter.as_ref(), &r, &out_path, claim, no_redact)?;

    let size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "✦ wrote {} — {count} message(s), {}",
        out_path.display(),
        human_bytes(size),
    );
    if !no_redact {
        eprintln!("{}", crate::cmd::view::redaction_summary(&stats));
    }
    Ok(())
}

fn share_session(
    adapter: &dyn cv_core::Adapter,
    r: &cv_core::SessionRef,
    out_path: &std::path::Path,
    claim: cv_core::publish::Claim,
    no_redact: bool,
) -> Result<(usize, RedactStats)> {
    // Never open the destination for writing: it may be a transcript, a hard link to one,
    // or an existing artifact. Publish only a complete, accepted read into a fresh name.
    let mut temporary = cv_core::publish::Staged::beside(out_path, "share", 0o600)?;
    let mut w = std::io::BufWriter::new(temporary.file());

    let mut sink = ShareSink {
        frames: Frames::new(&mut w),
        redact: !no_redact,
        opts: RedactOptions::default(),
        stats: RedactStats::default(),
        resolver: cv_core::Resolver::new(Some(r.path.clone())),
        harness: r.harness,
        id: r.id.clone(),
        cwd: r.cwd.clone(),
        created_at: r.created_at,
        title: r.title.clone(),
        model: None,
        first_prompt: None,
        meta_for_end: None,
        count: 0,
    };
    let pass = cv_core::stream::Pass {
        guard: true,
        ..cv_core::stream::Pass::RECORD
    };
    let guard = cv_core::stream::stream_counted(adapter, r, &Demand::LEAN, &mut sink, pass)?
        .guard
        .expect("guarded pass");
    sink.finish(env!("CARGO_PKG_VERSION"));

    let count = sink.count;
    let stats = std::mem::take(&mut sink.stats);
    let io_result = std::mem::replace(&mut sink.frames.result, Ok(()));
    drop(sink);
    io_result.with_context(|| format!("writing {}", out_path.display()))?;
    w.flush().with_context(|| format!("writing {}", out_path.display()))?;
    drop(w);
    guard.verify(r)?;
    temporary.publish(out_path, claim)?;

    Ok((count, stats))
}

/// Streaming sink: scrub + render each message as it arrives and write it through, holding back
/// only a small buffer until the header's label/model are known (they come from the first turns).
/// Header holdback uses the same frame writer as text rendering; this sink supplies HTML
/// metadata and redacts messages before they become frames.
struct ShareSink<'w, W: Write> {
    frames: Frames<'w, W>,
    redact: bool,
    opts: RedactOptions,
    stats: RedactStats,
    resolver: cv_core::Resolver,
    // Header facts: discovery-time values, refined by `meta()` / the first turns.
    harness: Harness,
    id: String,
    cwd: Option<PathBuf>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    title: Option<String>,
    model: Option<String>,
    first_prompt: Option<String>,
    /// The meta the header was written with, kept so the footer badges agree with it.
    meta_for_end: Option<ShareMeta>,
    count: usize,
}

impl<W: Write> ShareSink<'_, W> {
    fn flush_header(&mut self) {
        if self.frames.printed {
            return;
        }
        // The title can itself contain a secret; scrub it *before* label truncation so the
        // recognizers see the full token.
        let title = match (&self.title, self.redact) {
            (Some(t), true) => Some(redact_text(t)),
            (Some(t), false) => Some(t.clone()),
            (None, _) => None,
        };
        let meta = ShareMeta {
            label: label_from(title.as_deref(), self.first_prompt.as_deref()),
            harness: self.harness,
            id: self.id.clone(),
            model: self.model.clone(),
            cwd: self.cwd.as_deref().map(home_rel),
            created_at: self.created_at,
            redacted: self.redact,
        };
        self.frames.start(&html::share_begin(&meta));
        self.meta_for_end = Some(meta);
    }

    /// Flush any held-back header (short sessions) and write the footer/epilogue.
    fn finish(&mut self, version: &str) {
        self.flush_header();
        let meta = self.meta_for_end.take().expect("flush_header sets meta");
        let total = self.stats.total();
        self.frames.write(&html::share_end(&meta, total, version));
    }
}

impl<W: Write> MessageSink for ShareSink<'_, W> {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.frames.take_error()
    }
    fn meta(&mut self, s: &Session) {
        // Parsed metadata beats discovery-time guesses (same contract as `cv show`'s sink).
        self.title = s.title.clone();
        if self.model.is_none() {
            self.model = s.model.clone();
        }
        if self.cwd.is_none() {
            self.cwd = s.cwd.clone();
        }
        if self.created_at.is_none() {
            self.created_at = s.created_at;
        }
    }

    fn message(&mut self, mut m: Message) -> Flow {
        if self.frames.result.is_err() {
            return Flow::Stop;
        }
        if let Err(error) = m.materialize(&self.resolver).and_then(|()| {
            if self.redact {
                redact_message(&mut m, &self.opts, &mut self.stats, &self.resolver)?;
            }
            Ok(())
        }) {
            self.frames.result = Err(std::io::Error::other(error));
            return Flow::Stop;
        }
        if self.model.is_none() {
            if let Some(md) = &m.model {
                self.model = Some(md.clone());
            }
        }
        // The first prompt (post-redaction) backs the header label when there's no title.
        if self.first_prompt.is_none() {
            match m.prompt_title(&self.resolver) {
                Ok(title) => self.first_prompt = title,
                Err(error) => {
                    self.frames.result = Err(std::io::Error::other(error));
                    return Flow::Stop;
                }
            }
        }
        let mut rendered = String::new();
        if let Err(error) = html::share_message(&m, self.count, &mut rendered, &self.resolver) {
            self.frames.result = Err(std::io::Error::other(error));
            return Flow::Stop;
        }
        drop(m);
        self.count += 1;

        self.frames.push(rendered);
        if !self.frames.printed
            && Frames::<W>::ready(
                self.model.is_some(),
                self.title.is_some() || self.first_prompt.is_some(),
                self.count,
            )
        {
            self.flush_header();
        }
        if self.frames.result.is_err() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

/// Session ids become filenames; keep them filesystem-safe.
fn safe_stem(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::ir::Role;
    use cv_core::{Block, Json};

    #[test]
    fn failed_share_read_never_publishes_or_replaces_an_artifact() {
        struct Fails;
        impl cv_core::Adapter for Fails {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<cv_core::Discovered> {
                Ok(Default::default())
            }
            fn stream(&self, r: &cv_core::SessionRef, _: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
                let mut message = Message::new(Role::User);
                message.content.push(Block::from_text("partial output".into()));
                sink.try_message(message)?;
                anyhow::bail!("source failed after a message from {}", r.id)
            }
        }
        let world = crate::test_world("share-failed-read");
        let source = world.base.join("source.jsonl");
        std::fs::write(&source, "source").unwrap();
        let reference = cv_core::SessionRef::new("failed", Harness::Claude, &source);
        for existing in [false, true] {
            let output = world.base.join(format!("output-{existing}.html"));
            if existing {
                std::fs::write(&output, "prior artifact").unwrap();
            }
            assert!(share_session(&Fails, &reference, &output, cv_core::publish::Claim::Replace, false).is_err());
            if existing {
                assert_eq!(std::fs::read(&output).unwrap(), b"prior artifact");
            } else {
                assert!(!output.exists());
            }
        }
        assert!(std::fs::read_dir(&world.base).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".cv-share-")));
    }

    fn render(input: Json) -> Result<String> {
        let mut output = Vec::new();
        let mut sink = ShareSink {
            frames: Frames::new(&mut output),
            redact: true,
            opts: RedactOptions::default(),
            stats: RedactStats::default(),
            resolver: cv_core::Resolver::new(None),
            harness: Harness::Claude,
            id: "source-share".into(),
            cwd: None,
            created_at: None,
            title: Some("source parity".into()),
            model: None,
            first_prompt: None,
            meta_for_end: None,
            count: 0,
        };
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::ToolUse {
            id: "call".into(),
            name: "Bash".into(),
            input,
            namespace: None,
            extensions: Default::default(),
        });
        sink.try_message(message)?;
        sink.finish("test");
        if let Some(error) = sink.take_error() {
            return Err(error);
        }
        Ok(String::from_utf8(output).unwrap())
    }

    #[test]
    fn source_share_matches_owned_and_rejects_deleted_or_changed_arguments() {
        let world = crate::test_world("share-json-boundary");
        let path = world.base.join("arguments.json");
        let value =
            serde_json::json!({"command": "echo \"月🌙é\"\n".repeat(300), "api_key": "sk-abcDEF1234567890ghijkl"});
        let bytes = serde_json::to_vec(&value).unwrap();
        let expected = render(value.into()).unwrap();
        assert!(!expected.contains("sk-abcDEF1234567890ghijkl"));
        for deleted in [false, true] {
            std::fs::write(&path, &bytes).unwrap();
            let input = Json::source(&path, 0, bytes.len() as u64).unwrap();
            assert_eq!(render(input.clone()).unwrap(), expected);
            if deleted {
                std::fs::remove_file(&path).unwrap();
            } else {
                std::fs::write(&path, b"{} ").unwrap();
            }
            assert!(
                render(input).is_err(),
                "share cannot report a completed artifact for stale arguments"
            );
        }
    }
}
