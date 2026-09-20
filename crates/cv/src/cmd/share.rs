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

use crate::util::{home_rel, parse_harness, resolve};
use anyhow::{Context, Result};
use cv_core::html::{self, ShareMeta};
use cv_core::ir::{label_from, Harness, Message, Role, Session};
use cv_core::redact::{redact_message, redact_text, RedactOptions, RedactStats};
use cv_core::{Flow, MessageSink, ParseOptions};
use std::io::Write;
use std::path::PathBuf;

pub(crate) fn cmd_share(id: &str, harness: Option<String>, out: Option<PathBuf>, no_redact: bool) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = resolve(id, want)?;

    if no_redact {
        eprintln!("⚠ --no-redact: secrets and PII will NOT be scrubbed — review the file before sharing it");
    }

    let out_path = out.unwrap_or_else(|| PathBuf::from(format!("{}.html", safe_stem(&r.id))));
    let file = std::fs::File::create(&out_path).with_context(|| format!("creating {}", out_path.display()))?;
    let mut w = std::io::BufWriter::new(file);

    let mut sink = ShareSink {
        out: &mut w,
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
        first_user: None,
        buf: Vec::new(),
        printed: false,
        meta_for_end: None,
        count: 0,
        result: Ok(()),
    };
    adapter.stream(&r, &ParseOptions::bulk(), &mut sink)?;
    sink.finish(env!("CARGO_PKG_VERSION"));

    let count = sink.count;
    let stats = std::mem::take(&mut sink.stats);
    let io_result = std::mem::replace(&mut sink.result, Ok(()));
    drop(sink);
    io_result.with_context(|| format!("writing {}", out_path.display()))?;
    w.flush().with_context(|| format!("writing {}", out_path.display()))?;

    let size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "✦ wrote {} — {count} message(s), {}",
        out_path.display(),
        human_size(size),
    );
    if !no_redact {
        eprintln!(
            "✦ redacted {} item(s): {} api_key, {} private_key, {} jwt, {} email, {} blob, {} assignment",
            stats.total(),
            stats.api_keys,
            stats.private_keys,
            stats.jwts,
            stats.emails,
            stats.blobs,
            stats.assignments,
        );
    }
    Ok(())
}

/// Streaming sink: scrub + render each message as it arrives and write it through, holding back
/// only a small buffer until the header's label/model are known (they come from the first turns).
/// Mirrors the shape of `view.rs`'s `stream_session_render`, reimplemented here because the share
/// header needs different facts (created date, redaction flag) and each message must be *mutated*
/// (redacted) before rendering, which a `Fn(&Message) -> String` renderer can't do.
struct ShareSink<'w, W: Write> {
    out: &'w mut W,
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
    first_user: Option<String>,
    /// Rendered messages held back until the header is written.
    buf: Vec<String>,
    printed: bool,
    /// The meta the header was written with, kept so the footer badges agree with it.
    meta_for_end: Option<ShareMeta>,
    count: usize,
    result: std::io::Result<()>,
}

/// Stop buffering and write the header once we've seen this many messages, even if the model is
/// still unknown (matches `view.rs`'s holdback — header facts live in the first few turns).
const HOLDBACK: usize = 24;

impl<W: Write> ShareSink<'_, W> {
    fn write(&mut self, s: &str) {
        if self.result.is_ok() {
            self.result = self.out.write_all(s.as_bytes());
        }
    }

    fn flush_header(&mut self) {
        if self.printed {
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
            label: label_from(title.as_deref(), self.first_user.as_deref()),
            harness: self.harness,
            id: self.id.clone(),
            model: self.model.clone(),
            cwd: self.cwd.as_deref().map(home_rel),
            created_at: self.created_at,
            redacted: self.redact,
        };
        self.write(&html::share_begin(&meta));
        self.meta_for_end = Some(meta);
        let buffered = std::mem::take(&mut self.buf);
        for s in buffered {
            self.write(&s);
        }
        self.printed = true;
    }

    /// Flush any held-back header (short sessions) and write the footer/epilogue.
    fn finish(&mut self, version: &str) {
        self.flush_header();
        let meta = self.meta_for_end.take().expect("flush_header sets meta");
        let total = self.stats.total();
        self.write(&html::share_end(&meta, total, version));
    }
}

impl<W: Write> MessageSink for ShareSink<'_, W> {
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
        if self.result.is_err() {
            return Flow::Stop;
        }
        m.materialize(&self.resolver);
        if self.redact {
            redact_message(&mut m, &self.opts, &mut self.stats);
        }
        if self.model.is_none() {
            if let Some(md) = &m.model {
                self.model = Some(md.clone());
            }
        }
        // First user text (post-redaction) backs the header label when there's no title.
        if self.first_user.is_none() && m.role == Role::User {
            if let Some(t) = m.text() {
                if !t.trim().is_empty() {
                    self.first_user = Some(t);
                }
            }
        }
        let mut rendered = String::new();
        html::share_message(&m, self.count, &mut rendered);
        drop(m);
        self.count += 1;

        if self.printed {
            self.write(&rendered);
        } else {
            self.buf.push(rendered);
            let title_known = self.title.is_some() || self.first_user.is_some();
            if (self.model.is_some() && title_known) || self.buf.len() >= HOLDBACK {
                self.flush_header();
            }
        }
        if self.result.is_err() {
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

fn human_size(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}
