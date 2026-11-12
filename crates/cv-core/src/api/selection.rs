//! Selection shared by JSON, rendered transcripts and relevance excerpts. Payloads can stay lazy
//! messages or already bounded excerpts; the selector never resolves or renders them.
use std::collections::VecDeque;

pub struct MessageWindow<T> {
    pub idx: usize,
    pub start: usize,
    pub end: Option<usize>,
    mode: Mode<T>,
}
enum Mode<T> {
    Range,
    Tail {
        n: usize,
        ring: VecDeque<(usize, T)>,
    },
    Best {
        score: i64,
        previous: Option<(usize, T)>,
        selected: Vec<(usize, T)>,
        pending: bool,
    },
}
impl<T> MessageWindow<T> {
    pub fn new(start: usize, end: Option<usize>) -> Self {
        Self {
            idx: 0,
            start,
            end,
            mode: Mode::Range,
        }
    }
    pub fn tail(mut self, n: usize) -> Self {
        self.mode = Mode::Tail {
            n,
            ring: VecDeque::new(),
        };
        self
    }
    pub fn best() -> Self {
        Self {
            mode: Mode::Best {
                score: -1,
                previous: None,
                selected: Vec::new(),
                pending: false,
            },
            ..Self::new(0, None)
        }
    }
    pub fn is_tail(&self) -> bool {
        matches!(self.mode, Mode::Tail { .. })
    }
    pub fn contains(&self, index: usize) -> bool {
        index >= self.start && self.end.is_none_or(|end| index < end)
    }
    /// Advance the source ordinal even when a relevance projection excludes this message.
    pub fn skip(&mut self) {
        self.idx += 1;
    }
    pub fn push(&mut self, value: T) -> Option<(usize, T)> {
        let i = self.idx;
        self.idx += 1;
        match &mut self.mode {
            Mode::Range => self.contains(i).then_some((i, value)),
            Mode::Tail { n, ring } => {
                if *n > 0 {
                    ring.push_back((i, value));
                    if ring.len() > *n {
                        ring.pop_front();
                    }
                }
                None
            }
            Mode::Best { .. } => unreachable!("relevance windows need a score"),
        }
    }
    /// Finish a deferred selection; the returned source ordinals retain gaps in relevance excerpts.
    pub fn finish(&mut self) -> Vec<(usize, T)> {
        match std::mem::replace(&mut self.mode, Mode::Range) {
            Mode::Range => Vec::new(),
            Mode::Tail { ring, .. } => {
                self.start = self.idx - ring.len();
                ring.into_iter().collect()
            }
            Mode::Best { selected, .. } => selected,
        }
    }
}
impl<T: Clone> MessageWindow<T> {
    /// Highest scoring eligible message, its preceding eligible neighbor and one following
    /// neighbor. Ties retain the first window; a new winner replaces the pending old window.
    pub fn scored(&mut self, value: T, score: i64) {
        let i = self.idx;
        self.idx += 1;
        let Mode::Best {
            score: best,
            previous,
            selected,
            pending,
        } = &mut self.mode
        else {
            unreachable!("scored messages require a relevance window")
        };
        if score > *best {
            *best = score;
            selected.clear();
            selected.extend(previous.take());
            selected.push((i, value.clone()));
            *pending = true;
        } else if *pending {
            selected.push((i, value.clone()));
            *pending = false;
        }
        *previous = Some((i, value));
    }
}

/// Where `--around` (or `/messages?around=`) centres: a 0-based message index, or a message's own id.
///
/// Bare non-negative integers parse as [`Around::Index`]; `id:<raw>` forces [`Around::Id`] even
/// when `<raw>` is all digits (for numeric message IDs like Hermes SQLite rowids); any other
/// non-empty string parses as [`Around::Id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Around {
    Index(usize),
    Id(String),
}

pub fn parse_around(spec: &str) -> std::result::Result<Around, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("expected a message number or a message id".into());
    }
    if let Some(id) = spec.strip_prefix("id:") {
        if id.is_empty() {
            return Err("expected a message id after `id:`".into());
        }
        return Ok(Around::Id(id.to_owned()));
    }
    Ok(spec
        .parse::<usize>()
        .map_or_else(|_| Around::Id(spec.to_owned()), Around::Index))
}

/// The 0-based index of the message whose `id` is `id`, in the lean stream numbering every window
/// uses; `None` when the session holds no such message.
pub fn message_index(adapter: &dyn crate::Adapter, r: &crate::SessionRef, id: &str) -> anyhow::Result<Option<usize>> {
    struct Find<'a> {
        id: &'a str,
        at: usize,
        found: Option<usize>,
    }
    impl crate::stream::MessageSink for Find<'_> {
        fn message(&mut self, m: crate::Message) -> crate::stream::Flow {
            if m.id.as_deref() == Some(self.id) {
                self.found = Some(self.at);
                return crate::stream::Flow::Stop;
            }
            self.at += 1;
            crate::stream::Flow::Continue
        }
    }
    let mut find = Find { id, at: 0, found: None };
    adapter.try_stream(r, &crate::Demand::LEAN.with_spans(), &mut find)?;
    Ok(find.found)
}
