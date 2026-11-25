//! The JSON sink over the shared message selector: `cv show --json` (and so `cv mcp`'s `show`), `cv export`'s JSON
//! forms and `GET /api/session/{h}/{id}/messages` all select messages through this sink. The
//! window is `[start, end)`, or the last `n`, cut at a byte budget; selected messages are staged
//! as RS-framed compact JSON ([`crate::projection::write_frame`]) and out-of-window messages pass
//! by as lazy handles, never materialized.
use super::MessageWindow;
use crate::bytes::{ByteSource, ByteStage};
use crate::source::ReadSet;
use crate::stream::{Checkpoint, Flow, MessageSink, Tier};
use crate::{Message, Resolver, Session};
use anyhow::{Context as _, Result};
use serde_json::Value;
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};

pub struct WindowSink {
    opensession: bool,
    resolver: Resolver,
    primary: PathBuf,
    window: MessageWindow<Message>,
    stage: Option<BufWriter<ByteStage>>,
    budget: Option<usize>,
    bytes: usize,
    pub selected: usize,
    /// Selected messages that are verbatim non-conversational carriers (counted in `selected`).
    pub carriers: usize,
    /// The first in-window message the budget rejected: the continuation point.
    pub next: Option<usize>,
    /// Stop at `end` after noting a message exists there (`more`), instead of reading on to EOF
    /// for session facts written later (the `/messages` route pages; the CLI reads whole).
    stop_at_end: bool,
    pub more: bool,
    /// Keep the selected messages materialized too (redaction needs the decoded tree).
    kept: Option<Vec<Message>>,
    /// Session metadata the stream delivered (codex meta / the parse-bridge), when any did.
    pub(super) meta: Option<Value>,
    fail: Option<anyhow::Error>,
    pub(super) capture: bool,
    pub(super) checkpoint: Option<(usize, Checkpoint, Option<Value>)>,
    pub(super) reads: ReadSet,
}

impl WindowSink {
    /// A window over `primary`'s session, read to EOF, frames staged.
    pub fn new(primary: &Path, start: usize, end: Option<usize>) -> Self {
        Self {
            opensession: false,
            resolver: Resolver::new(Some(primary.to_path_buf())),
            primary: primary.to_path_buf(),
            window: MessageWindow::new(start, end),
            stage: Some(BufWriter::with_capacity(64 * 1024, ByteStage::default())),
            budget: None,
            bytes: 0,
            selected: 0,
            carriers: 0,
            next: None,
            stop_at_end: false,
            more: false,
            kept: None,
            meta: None,
            fail: None,
            capture: false,
            checkpoint: None,
            reads: ReadSet::default(),
        }
    }

    /// Emit original OpenSession carrier records in selected document frames.
    pub fn for_document(mut self, document: bool) -> Self {
        self.opensession = document;
        self
    }

    /// The API's page: stops at `end`, may capture a checkpoint there for the next page.
    pub(super) fn page(primary: &Path, start: usize, end: Option<usize>, capture: bool, reads: ReadSet) -> Self {
        Self {
            stop_at_end: true,
            capture,
            reads,
            ..Self::new(primary, start, end)
        }
    }

    /// Retain initial header facts while publishing the latest evidence from the read.
    pub(super) fn finish_meta(&mut self, session: &Session) {
        match &mut self.meta {
            Some(meta) => super::update_read_coverage(meta, session),
            None => self.meta = Some(super::session_meta_json(session)),
        }
    }

    /// Select the last `n` messages instead of `[start, end)`.
    pub fn tail(mut self, n: usize) -> Self {
        self.window = MessageWindow::new(self.start, self.end).tail(n);
        self
    }

    /// Stop before the in-window message that would push the staged bytes past `budget`; the
    /// first in-window message always fits.
    pub fn budget(mut self, budget: Option<usize>) -> Self {
        self.budget = budget;
        self
    }

    /// Materialize and retain selected messages instead of staging their payload frames.
    /// Budgeting still measures their serialized size; retrieve the messages with [`Self::take_kept`].
    /// Configure this before streaming begins.
    pub fn keeping(mut self) -> Self {
        self.kept = Some(Vec::new());
        self
    }

    fn accept(&mut self, mut message: Message, index: usize) -> Result<()> {
        if self.next.is_some() {
            return Ok(());
        }
        let remaining = (self.selected > 0)
            .then(|| self.budget.map(|budget| budget.saturating_sub(self.bytes)))
            .flatten();
        let stage = self.stage.as_mut().context("window frames already taken")?;
        // Pending bytes belong to the same spool; do not flush each frame.
        let before = stage.get_ref().len() + stage.buffer().len() as u64;
        let size = match self.kept.as_mut() {
            None => match crate::harness::opensession::opaque_message(&message).filter(|_| self.opensession) {
                Some(original) => {
                    crate::projection::write_frame_with(stage, remaining, |w| Ok(serde_json::to_writer(w, original)?))?
                }
                None => crate::projection::write_frame(stage, &message, &self.resolver, remaining)?,
            },
            Some(_) => {
                message.materialize(&self.resolver)?;
                crate::projection::write_frame(&mut std::io::sink(), &message, &self.resolver, remaining)?
            }
        };
        if self.selected > 0
            && self
                .budget
                .is_some_and(|budget| size > budget.saturating_sub(self.bytes))
        {
            stage.flush()?;
            stage.get_mut().truncate(before)?;
            self.next = Some(index);
            return Ok(());
        }
        self.bytes = self.bytes.checked_add(size).context("JSON budget size overflow")?;
        self.selected += 1;
        self.carriers += usize::from(message.is_carrier());
        if let Some(kept) = self.kept.as_mut() {
            kept.push(message);
        }
        Ok(())
    }

    /// Replay a kept tail through the window and hand over the staged frames. The sink's counts
    /// (`selected`, `next`, `more`, `idx`) stay readable.
    pub fn finish(&mut self) -> Result<ByteSource> {
        if let Some(error) = self.fail.take() {
            return Err(error);
        }
        for (i, message) in self.window.finish() {
            self.accept(message, i)?;
        }
        let stage = self.stage.take().context("window frames already taken")?;
        Ok(stage.into_inner().map_err(|error| error.into_error())?.finish()?)
    }

    /// The materialized selection of a [`Self::keeping`] window.
    pub fn take_kept(&mut self) -> Vec<Message> {
        self.kept.take().unwrap_or_default()
    }

    /// The staged frames as values (the unstaged `/messages` reply). One frame decodes at a
    /// time, straight off the staged source: the reply never duplicates the staged bytes.
    pub(super) fn values(&mut self) -> Result<Vec<Value>> {
        let frames = self.finish()?;
        let mut reader = std::io::BufReader::new(frames.reader()?);
        let mut values = Vec::new();
        let mut frame = Vec::new();
        loop {
            frame.clear();
            if reader.read_until(0x1e, &mut frame)? == 0 {
                return Ok(values);
            }
            if frame.last() == Some(&0x1e) {
                frame.pop();
            }
            if !frame.is_empty() {
                values.push(serde_json::from_slice(&frame)?);
            }
        }
    }

    fn in_window(&self, index: usize) -> bool {
        self.window.contains(index)
    }
}

impl MessageSink for WindowSink {
    fn tier(&self) -> Tier {
        if self.window.is_tail() || (self.fail.is_none() && self.next.is_none() && self.in_window(self.idx)) {
            Tier::Rich
        } else {
            Tier::Facts
        }
    }
    fn dependency(&mut self, path: &Path) {
        self.reads.record(path);
    }
    fn wants_checkpoint(&mut self, _: u64) -> bool {
        self.capture && self.end.is_some_and(|end| end > self.start && self.idx == end)
    }
    fn checkpoint(&mut self, checkpoint: &mut Checkpoint) {
        self.capture = false;
        self.checkpoint = Some((self.idx, checkpoint.clone(), self.meta.clone()));
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.fail.take()
    }
    fn meta(&mut self, s: &Session) {
        self.finish_meta(s);
        if s.source_path.is_some() {
            self.resolver = s.resolver();
        }
    }
    fn message(&mut self, m: Message) -> Flow {
        crate::source::record_message_reads(&m, Some(&self.primary), &mut self.reads);
        let index = self.idx;
        if self.fail.is_some() {
            return Flow::Stop;
        }
        let selected = self.window.push(m);
        if !self.window.is_tail() && self.end.is_some_and(|end| index >= end) && self.stop_at_end {
            self.more = true;
            return Flow::Stop;
        }
        let Some((index, m)) = selected else {
            return Flow::Continue;
        };
        match self.accept(m, index) {
            Ok(()) => Flow::Continue,
            Err(error) => {
                self.fail = Some(error);
                Flow::Stop
            }
        }
    }
}

impl std::ops::Deref for WindowSink {
    type Target = MessageWindow<Message>;
    fn deref(&self) -> &Self::Target {
        &self.window
    }
}
impl std::ops::DerefMut for WindowSink {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.window
    }
}
