//! What a summary carries verbatim from one generation to the next, never re-summarized: the
//! person's rulings, the open promises, the terms the work uses, the self and setup facts, the
//! live identifiers, and pointers to the standing rules the work touched.
//!
//! Rules come in three layers, and cv re-introduces only the middle one. The base layer
//! (instruction files, memory, skills) is re-injected by the harness after every compaction, so it
//! is never copied. The per-turn layer (rules a hook injects when a turn calls for them) keeps
//! firing after the seam, so it is not duplicated either. What a compaction loses is the
//! contextual layer: the person's rulings in the span, verbatim (a ruling that was never filed
//! anywhere exists only there), and which standing rules the span's work touched, carried as
//! pointers ([`Pointer`]: the rule's id, never its text). The selection is deterministic: a
//! ruling must quote a message the person wrote, outside the harness's markup, and a pointer is an
//! id the configured patterns found in the span; each records why it was chosen.
//!
//! Loss compounds over summary generations: a compactor asked to keep side constraints keeps few
//! of them, while a separate extractor whose output is appended after the summary keeps most
//! (arXiv 2608.11242). So the narrative summary and these sections are two passes: the extraction
//! pass returns quotes, each quote is kept only when it is found verbatim in the message it cites
//! ([`verify`], so an extractor cannot plant a fact the transcript does not hold), and the kept
//! items are merged into the previous generation's ([`Carry::merge`]) instead of being
//! summarized again. Identifiers are read from the span by shape ([`ids`]), with no model, and so
//! are the lines of the span that state a sha, a row or a count ([`lines`]): a blind probe found
//! the narrative thins them (this window's items 2 of 6, native 4), and a line copied whole keeps
//! the number with what it counts.
//!
//! A session that moves to cv from its harness's own compaction brings a summary cv never wrote,
//! with nothing of it in any carry: rulings, schedules, handles and balances the harness kept,
//! which the narrative is told not to list and a ruling may not quote. So the extraction also
//! copies out the lines of that summary that still bind ([`Section::Standing`]), verified against
//! it, bounded in bytes ([`Limits::standing_bytes`]), and marked as of that summary.
//!
//! The carry is bounded. A pointer is made only from what the agent itself did with a rule (named
//! it in a reply or a tool call, or drew a refusal naming it), never from a rule a hook merely
//! injected, and only the most recently used are kept ([`Limits::pointers`]). An item no span
//! has said or used again for [`Limits::retire_after`] windows retires; a ruling of the person's
//! never retires by age, only when a later ruling of theirs supersedes it. Every `[m:ID]` the
//! narrative cites must be a message of the window, written in full ([`cites`]).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Which carried section an item belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Section {
    /// An instruction from the person (or their configuration) that still binds the work.
    Rule,
    /// Something the agent said it would do, or was asked to do, not yet done.
    Promise,
    /// A term the work uses with a meaning of its own, and that meaning.
    Term,
    /// A fact about the agent itself or its setup (model, context size, seat, host, paths).
    Fact,
    /// A line of the harness's own earlier summary that still binds the work: what a session
    /// that moved to cv kept from before cv ran.
    Standing,
}

impl Section {
    const ALL: [Section; 5] = [
        Section::Rule,
        Section::Promise,
        Section::Term,
        Section::Fact,
        Section::Standing,
    ];

    fn name(self) -> &'static str {
        match self {
            Section::Rule => "rule",
            Section::Promise => "promise",
            Section::Term => "term",
            Section::Fact => "fact",
            Section::Standing => "standing",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Section::Rule => "The person's rulings in force",
            Section::Promise => "Open promises",
            Section::Term => "Terms",
            Section::Fact => "Self and setup facts",
            Section::Standing => {
                "From the harness's own earlier summary, as of that summary (probe before relying on one)"
            }
        }
    }
}

/// One carried item: a quote, verbatim from the message `m` cites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub section: Section,
    pub quote: String,
    /// The id of the message the quote is from.
    pub m: String,
    /// Why it is carried: said in which generation's span.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
    /// The window (counted in compactions before it) a span last said or used the item in:
    /// what its retirement is counted from. `None` in a record written before the count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<usize>,
}

/// A standing rule the span's work touched: its id as the configured patterns found it (a rule
/// store id, an instruction name), where it was last seen and how often. The text is never
/// copied: the reader looks the id up where such rules live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pointer {
    pub id: String,
    pub last: String,
    pub hits: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
}

/// An identifier seen in the span (a commit sha, a task row, a path, a URL), with the first and
/// the last message it appears in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Id {
    pub id: String,
    pub first: String,
    pub last: String,
}

/// Where a [`Line`] was copied from, in the order a tight budget keeps them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The agent's own prose.
    Said,
    /// A command or a tool call's arguments the agent wrote (a message it posted, a commit).
    Ran,
    /// What a tool answered (the head and tail the summary model reads).
    Read,
}

/// One line of a span that states a sha, a row or a count, copied whole with no model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub line: String,
    /// The id of the message it is from.
    pub m: String,
    pub from: Source,
    /// The window (counted in compactions before it) the line was read in.
    pub window: usize,
}

/// What one span adds to the carry, before the merge.
#[derive(Debug, Clone, Default)]
pub struct Span {
    pub items: Vec<Item>,
    pub ids: Vec<Id>,
    /// The span's lines stating a sha, a row or a count ([`lines`]).
    pub lines: Vec<Line>,
    pub pointers: Vec<Pointer>,
    /// What the span did to carried items, each quoted as carried ([`Marks`], verified).
    pub marks: Marks,
    /// The window the span is in: compactions before it.
    pub window: usize,
}

/// What a span did to the items carried into it, each quoted as carried: the promises it
/// fulfilled, the items it said or used again, the person's rulings a later ruling of theirs
/// replaced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marks {
    pub done: Vec<String>,
    pub used: Vec<String>,
    pub superseded: Vec<String>,
}

/// How much a carry holds: the identifiers and pointers most recently seen, and how many windows
/// an item stays without being said or used again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub ids: usize,
    pub pointers: usize,
    pub retire_after: usize,
    /// Bytes of [`Section::Standing`] lines carried, the most recently seen kept.
    pub standing_bytes: usize,
    /// Bytes of [`Line`]s carried: this window's first, the agent's own words before its commands
    /// before what tools answered, the newest of each kept.
    pub lines_bytes: usize,
}

/// The carried sections of one record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Carry {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<Item>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<Id>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pointers: Vec<Pointer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<Line>,
}

/// What the extraction pass returned that was not kept: quotes not found verbatim in the message
/// they cite, and entries that were not items at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dropped {
    pub unverified: usize,
    pub malformed: usize,
    /// Quotes from the layers cv does not re-introduce: a ruling not in a message the person
    /// wrote (or inside harness markup there), any item quoting injected context.
    pub outside_layer: usize,
    /// Quotes shaped like a log or listing line (a leading timestamp, a `unit[pid]:` tag, a
    /// one-line commit): output the work read, not a statement to carry.
    pub noise: usize,
    /// Marks on carried items that name no carried item, or cite a message that cannot make
    /// them (a supersession not from the person, a use outside the span).
    pub marks: usize,
}

/// An extraction entry as the model writes it: `{"section", "quote", "m"}`, where `section` may
/// also mark a carried item, quoted as carried: `done` (a promise the span fulfilled), `used`
/// (an item the span said or used again), `superseded` (a ruling a later one replaced).
#[derive(Debug, Deserialize)]
struct Raw {
    section: String,
    quote: String,
    m: String,
}

/// A mark on a carried item as the extraction pass returned it, before [`verify_marks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub kind: MarkKind,
    /// The carried item, quoted as carried.
    pub quote: String,
    /// The message that fulfilled, used or superseded it.
    pub m: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    Done,
    Used,
    Superseded,
}

/// The extraction pass's answer: the first JSON array in `text` (a model may wrap it in prose or
/// a code fence). Returns the items and the marks on carried items.
pub fn parse(text: &str) -> (Vec<Item>, Vec<Mark>, Dropped) {
    let mut dropped = Dropped::default();
    let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) else {
        return (Vec::new(), Vec::new(), dropped);
    };
    if end < start {
        return (Vec::new(), Vec::new(), dropped);
    }
    let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(&text[start..=end]) else {
        dropped.malformed += 1;
        return (Vec::new(), Vec::new(), dropped);
    };
    let (mut items, mut marks) = (Vec::new(), Vec::new());
    for v in values {
        let Ok(raw) = serde_json::from_value::<Raw>(v) else {
            dropped.malformed += 1;
            continue;
        };
        let section = match raw.section.as_str() {
            "rule" => Section::Rule,
            "promise" => Section::Promise,
            "term" => Section::Term,
            "fact" => Section::Fact,
            "standing" => Section::Standing,
            mark @ ("done" | "used" | "superseded") => {
                marks.push(Mark {
                    kind: match mark {
                        "done" => MarkKind::Done,
                        "used" => MarkKind::Used,
                        _ => MarkKind::Superseded,
                    },
                    quote: raw.quote,
                    m: raw.m,
                });
                continue;
            }
            _ => {
                dropped.malformed += 1;
                continue;
            }
        };
        items.push(Item {
            section,
            quote: raw.quote,
            m: raw.m,
            why: String::new(),
            seen: None,
        });
    }
    (items, marks, dropped)
}

fn normalized(text: &str) -> String {
    let mut out = String::new();
    super::anchor::normalize_into(&mut out, text);
    out
}

/// Which messages of the span are whose: the person's prompts, context the harness or a hook
/// injected, the agent's own replies, and the harness's own summaries.
#[derive(Debug, Clone, Default)]
pub struct Layers {
    pub prompts: std::collections::HashSet<String>,
    pub injected: std::collections::HashSet<String>,
    /// The agent's replies: a self or setup fact quoted from one is what the agent assumed, not
    /// where the fact was shown, so it is not carried.
    pub replies: std::collections::HashSet<String>,
    /// The summaries the harness wrote itself: the one layer a [`Section::Standing`] line quotes.
    pub summaries: std::collections::HashSet<String>,
}

/// Keep only the items whose quote is in the message they cite, whitespace aside, and from the
/// layer cv re-introduces: no item quotes injected context, a ruling quotes a prompt the person
/// wrote, outside the harness's markup in it, a self or setup fact never quotes the agent, and a
/// standing line quotes a summary the harness wrote. `texts` maps a message id to its text as the
/// span holds it.
pub fn verify(items: Vec<Item>, texts: &HashMap<String, String>, layers: &Layers, dropped: &mut Dropped) -> Vec<Item> {
    items
        .into_iter()
        .filter(|item| {
            let ruling = item.section == Section::Rule;
            let standing = item.section == Section::Standing;
            if !ruling && !standing && noise(&item.quote) {
                dropped.noise += 1;
                return false;
            }
            if layers.injected.contains(&item.m)
                || ruling && !layers.prompts.contains(&item.m)
                || standing && !layers.summaries.contains(&item.m)
                || item.section == Section::Fact && layers.replies.contains(&item.m)
            {
                dropped.outside_layer += 1;
                return false;
            }
            let quote = normalized(&item.quote);
            let found = !quote.is_empty()
                && texts.get(&item.m).is_some_and(|t| {
                    let t = if ruling {
                        super::anchor::without_markup(t)
                    } else {
                        t.clone()
                    };
                    normalized(&t).contains(&quote)
                });
            if !found {
                dropped.unverified += 1;
            }
            found
        })
        .collect()
}

/// A quote shaped like a line of output rather than a statement: it opens with a timestamp
/// (`Thu 2026-10-29 00:05:00`, `2026-10-29T07:44`, `Oct 29 07:44:01`, `07:44:01`), carries a
/// `unit[pid]:` tag, or is a one-line commit (`13439a7d13 fix(...): ...`).
pub fn noise(quote: &str) -> bool {
    use std::sync::OnceLock;
    static SHAPE: OnceLock<regex::Regex> = OnceLock::new();
    let shape = SHAPE.get_or_init(|| {
        regex::Regex::new(
            r"(?x)
            ^\[?(?:(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun)[a-z]*,?\s+)?
               (?:[0-9]{4}-[0-9]{2}-[0-9]{2}[T\s][0-9]{2}:[0-9]{2}
                 | (?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)[a-z]*\s+[0-9]{1,2}\s+[0-9]{2}:[0-9]{2}
                 | [0-9]{2}:[0-9]{2}:[0-9]{2})
          | \b[A-Za-z0-9_.@-]+\[[0-9]+\]:
          | ^[0-9a-f]{7,40}\s+[a-z]+(?:\([^)]*\))?!?:\s",
        )
        .expect("the noise shapes compile")
    });
    shape.is_match(quote.trim_start())
}

/// The marks the span can make: each names an item carried into it (quoted as carried, whitespace
/// aside) and cites a message that can make it: a supersession is the person's own prompt, a use
/// or a fulfilment a message of the span. The rest are counted in `dropped`.
pub fn verify_marks(
    marks: Vec<Mark>,
    carried: &Carry,
    texts: &HashMap<String, String>,
    layers: &Layers,
    dropped: &mut Dropped,
) -> Marks {
    let mut out = Marks::default();
    for mark in marks {
        let quote = normalized(&mark.quote);
        let section = carried
            .items
            .iter()
            .find(|i| normalized(&i.quote) == quote)
            .map(|i| i.section);
        let ok = match (mark.kind, section) {
            (MarkKind::Done, Some(Section::Promise)) | (MarkKind::Used, Some(_)) => texts.contains_key(&mark.m),
            (MarkKind::Superseded, Some(Section::Rule)) => layers.prompts.contains(&mark.m),
            _ => false,
        };
        if !ok {
            dropped.marks += 1;
            continue;
        }
        match mark.kind {
            MarkKind::Done => out.done.push(quote),
            MarkKind::Used => out.used.push(quote),
            MarkKind::Superseded => out.superseded.push(quote),
        }
    }
    out
}

/// The carried items the span says again, word for word (whitespace aside): `texts` are the
/// span's messages. A use the extraction pass did not mark still counts.
pub fn said_again(carried: &Carry, texts: &[(String, String)]) -> Vec<String> {
    let span: Vec<String> = texts.iter().map(|(_, t)| normalized(t)).collect();
    carried
        .items
        .iter()
        .map(|i| normalized(&i.quote))
        .filter(|q| !q.is_empty() && span.iter().any(|t| t.contains(q.as_str())))
        .collect()
}

/// What checking the narrative's citations found: cites kept as written, cites written as a
/// prefix of one message's id and completed, and cites of no message of the window, removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cites {
    pub kept: usize,
    pub completed: usize,
    pub unresolved: usize,
}

/// The narrative with every `[m:ID]` resolved against `known` (the window's message ids): a full
/// id stays, a prefix of at least 8 characters that names exactly one message is completed, and
/// anything else is removed, so no cite sends the reader to a message that is not there.
pub fn cites(narrative: &str, known: &std::collections::HashSet<String>) -> (String, Cites) {
    use std::sync::OnceLock;
    static CITE: OnceLock<regex::Regex> = OnceLock::new();
    let cite = CITE.get_or_init(|| regex::Regex::new(r"\[m:([^\]\s]*)\]").expect("the cite shape compiles"));
    let mut sorted: Vec<&str> = known.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut counts = Cites::default();
    let out = cite.replace_all(narrative, |c: &regex::Captures| {
        let id = &c[1];
        if known.contains(id) {
            counts.kept += 1;
            return c[0].to_owned();
        }
        if id.len() >= 8 {
            let from = sorted.partition_point(|k| *k < id);
            let mut hits = sorted[from..].iter().take_while(|k| k.starts_with(id));
            if let (Some(full), None) = (hits.next(), hits.next()) {
                counts.completed += 1;
                return format!("[m:{full}]");
            }
        }
        counts.unresolved += 1;
        String::new()
    });
    (out.into_owned(), counts)
}

/// The pointers `found` (`(message id, rule id)`, oldest first) make: one per rule id, with its
/// last message and count, from the least to the most recently used.
pub fn pointers(found: &[(String, String)]) -> Vec<Pointer> {
    let mut out: Vec<Pointer> = Vec::new();
    for (m, id) in found {
        match out.iter().position(|p| &p.id == id) {
            Some(at) => {
                let mut p = out.remove(at);
                p.last = m.clone();
                p.hits += 1;
                out.push(p);
            }
            None => out.push(Pointer {
                id: id.clone(),
                last: m.clone(),
                hits: 1,
                why: String::new(),
            }),
        }
    }
    out
}

/// The identifiers in `messages` (`(id, text)`, oldest first) by shape: commit shas (7 to 40 hex
/// digits holding a digit and a letter), task rows (`task/N`), absolute paths, URLs. Each with
/// the first and last message it appears in, in order of first appearance.
pub fn ids(messages: &[(String, String)]) -> Vec<Id> {
    use std::sync::OnceLock;
    static SHAPE: OnceLock<regex::Regex> = OnceLock::new();
    let shape = SHAPE.get_or_init(|| {
        regex::Regex::new(
            r"(?x)
            https?://[^\s<>()\[\]`'\x22]+
          | (?:^|[\s`'\x22(\[])(/(?:[A-Za-z0-9._@+-]+/)+[A-Za-z0-9._@+-]*)
          | \btask/[0-9]+\b
          | \b[0-9a-f]{7,40}\b",
        )
        .expect("the id shapes compile")
    });
    let mut out: Vec<Id> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for (m, text) in messages {
        for c in shape.captures_iter(text) {
            let id = c.get(1).or_else(|| c.get(0)).map_or("", |g| g.as_str());
            let id = id.trim_end_matches(['.', ',', ':', ';']);
            let hex = id.bytes().all(|b| b.is_ascii_hexdigit());
            if id.is_empty()
                || hex && !(id.bytes().any(|b| b.is_ascii_digit()) && id.bytes().any(|b| b.is_ascii_alphabetic()))
            {
                continue;
            }
            match at.get(id) {
                Some(&i) => out[i].last = m.clone(),
                None => {
                    at.insert(id.to_owned(), out.len());
                    out.push(Id {
                        id: id.to_owned(),
                        first: m.clone(),
                        last: m.clone(),
                    });
                }
            }
        }
    }
    out
}

/// The longest line [`lines`] copies: past it a line is a dump, not a statement.
const LINE_MAX: usize = 400;

/// Whether a line states a sha, a row (`issue/204`) or a count: `9 of 12`, `130 tests`, `$1.51`,
/// `42%`, `61 ms`.
fn states_a_measure(line: &str) -> bool {
    use std::sync::OnceLock;
    static SHAPE: OnceLock<regex::Regex> = OnceLock::new();
    let shape = SHAPE.get_or_init(|| {
        regex::Regex::new(
            r"(?xi)
            \b[a-z][a-z0-9-]*/[0-9]+\b
          | \b[0-9][0-9,.]*\s*(?:of|/)\s*[0-9]
          | \b[0-9][0-9,.]*\s?(?:pass(?:ed|es)?|fail(?:ed|s|ures?)?|tests?|reds?|rows?|modules?|lands?|errors?|miss(?:es)?|seats?|ms|min|kb|mb|gb|k\b)
          | [0-9]%
          | \$[0-9]",
        )
        .expect("the measure shapes compile")
    });
    shape.is_match(line) || is_sha_in(line)
}

/// Whether a line holds a commit sha: 7 to 40 hex digits holding a digit and a letter.
fn is_sha_in(line: &str) -> bool {
    line.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        (7..=40).contains(&w.len())
            && w.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && w.bytes().any(|b| b.is_ascii_digit())
            && w.bytes().any(|b| b.is_ascii_alphabetic())
    })
}

/// A rendered tool call's arguments as text: the JSON string escapes for a newline and a quote
/// undone, so a command or a posted message splits into its lines.
fn unescaped(args: &str) -> String {
    args.replace("\\n", "\n").replace("\\\"", "\"")
}

/// The lines of `turns` (`(role, id, text)`, rendered and oldest first: the agent's prose and
/// `[tool call: …]` blocks, and each `[tool result] …`) that state a sha, a row or a count, by
/// shape and with no model. The person's prompts and injected context are not read: their words
/// are carried whole elsewhere, and a hook's numbers are not this work's. A repeated line keeps its
/// newest place.
pub fn lines(turns: &[(&str, String, String)], window: usize) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut take = |text: &str, m: &str, from: Source| {
        for raw in text.lines() {
            let line = raw
                .trim()
                .trim_start_matches(['-', '*', '|', '#', ' '])
                .trim_end_matches(['|', ' ', '"', '}', ']']);
            if line.is_empty()
                || line.len() > LINE_MAX
                || !line.bytes().any(|b| b.is_ascii_alphabetic())
                || noise(line)
                || !states_a_measure(line)
            {
                continue;
            }
            out.retain(|l| l.line != line);
            out.push(Line {
                line: line.to_owned(),
                m: m.to_owned(),
                from,
                window,
            });
        }
    };
    for (role, m, text) in turns {
        if *role == "assistant" {
            // A rendered call is one line of JSON, or two around the cut `prompt::clip` marks.
            let mut in_call = false;
            for raw in text.lines() {
                if let Some(call) = raw.strip_prefix("[tool call: ") {
                    let args = call.split_once("\":\"").map_or(call, |(_, value)| value);
                    take(&unescaped(args), m, Source::Ran);
                    in_call = true;
                } else if in_call && (raw.starts_with("[… ") || raw.ends_with("\"}]")) {
                    take(&unescaped(raw), m, Source::Ran);
                    in_call = raw.starts_with("[… ");
                } else {
                    take(raw, m, Source::Said);
                    in_call = false;
                }
            }
        } else if let Some(result) = text.strip_prefix("[tool result] ") {
            take(result, m, Source::Read);
        }
    }
    out
}

/// `lines` within `budget` bytes, in their order: this window's before earlier ones, then the
/// agent's own words before its commands before what tools answered, the newest of each kept.
fn within_lines_budget(lines: Vec<Line>, budget: usize, window: usize) -> Vec<Line> {
    let mut order: Vec<usize> = (0..lines.len()).collect();
    order.sort_by_key(|&i| (lines[i].window != window, lines[i].from, std::cmp::Reverse(i)));
    let mut used = 0usize;
    let mut keep = vec![false; lines.len()];
    for i in order {
        let cost = lines[i].line.len() + lines[i].m.len() + 8;
        if used + cost <= budget {
            used += cost;
            keep[i] = true;
        }
    }
    lines
        .into_iter()
        .zip(keep)
        .filter_map(|(line, keep)| keep.then_some(line))
        .collect()
}

/// `items` less the standing lines past `budget` bytes: the most recently seen stay, the order of
/// the rest kept. What falls out stays one `cv show` away at the message each line cites.
fn within_standing_budget(items: Vec<Item>, budget: usize) -> Vec<Item> {
    let mut standing: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].section == Section::Standing)
        .collect();
    standing.sort_by_key(|&i| std::cmp::Reverse(items[i].seen));
    let mut used = 0usize;
    let mut out_of_budget = std::collections::HashSet::new();
    for i in standing {
        let cost = items[i].quote.len();
        if used + cost > budget {
            out_of_budget.insert(i);
        } else {
            used += cost;
        }
    }
    items
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !out_of_budget.contains(i))
        .map(|(_, item)| item)
        .collect()
}

impl Carry {
    /// The next generation: this one's items and ids, then the span's new ones (an item already
    /// carried, whitespace aside, is not repeated), less the promises the span fulfilled, the
    /// rulings a later ruling superseded, and the items no span has said or used for
    /// `limits.retire_after` windows (a ruling never retires by age); the id and pointer lists and
    /// the standing lines keep the most recently seen. Nothing carried is rewritten.
    pub fn merge(&self, span: Span, limits: &Limits) -> Carry {
        let Span {
            items,
            ids,
            lines,
            pointers,
            marks,
            window,
        } = span;
        let mut seen: std::collections::HashSet<(Section, String)> = std::collections::HashSet::new();
        let mut next = Vec::new();
        let fresh: Vec<Item> = items
            .into_iter()
            .map(|mut i| {
                i.why = format!("said in this span at [m:{}]", i.m);
                i.seen = Some(window);
                i
            })
            .collect();
        let again: std::collections::HashSet<String> = fresh
            .iter()
            .map(|i| normalized(&i.quote))
            .chain(marks.used.iter().cloned())
            .collect();
        let carried = self.items.iter().cloned().map(|mut i| {
            if !i.why.starts_with("carried") {
                i.why = format!("carried from an earlier summary; first said at [m:{}]", i.m);
            }
            if i.seen.is_none() || again.contains(&normalized(&i.quote)) {
                i.seen = Some(window);
            }
            i
        });
        for item in carried.chain(fresh) {
            let key = (item.section, normalized(&item.quote));
            let gone = match item.section {
                Section::Promise if marks.done.contains(&key.1) => true,
                Section::Rule => marks.superseded.contains(&key.1),
                _ => item
                    .seen
                    .is_some_and(|s| window.saturating_sub(s) >= limits.retire_after),
            };
            if gone {
                continue;
            }
            if seen.insert(key) {
                next.push(item);
            }
        }
        let next = within_standing_budget(next, limits.standing_bytes);
        // An id seen again moves to the end: the list runs from least to most recently seen.
        let mut all = self.ids.clone();
        for mut id in ids {
            if let Some(at) = all.iter().position(|i| i.id == id.id) {
                id.first = all.remove(at).first;
            }
            all.push(id);
        }
        if all.len() > limits.ids {
            all.drain(..all.len() - limits.ids);
        }
        let mut rules: Vec<Pointer> = self
            .pointers
            .iter()
            .cloned()
            .map(|mut p| {
                p.why = format!("carried: the work touched it earlier, last at [m:{}]", p.last);
                p
            })
            .collect();
        for mut p in pointers {
            p.why = format!(
                "the work in this span touched it {} time(s), last at [m:{}]",
                p.hits, p.last
            );
            // A pointer used again moves to the end: the list runs from least to most recently used.
            if let Some(at) = rules.iter().position(|r| r.id == p.id) {
                rules.remove(at);
            }
            rules.push(p);
        }
        if rules.len() > limits.pointers {
            rules.drain(..rules.len() - limits.pointers);
        }
        let mut kept = self.lines.clone();
        for line in lines {
            kept.retain(|l| l.line != line.line);
            kept.push(line);
        }
        Carry {
            items: next,
            ids: all,
            pointers: rules,
            lines: within_lines_budget(kept, limits.lines_bytes, window),
        }
    }

    /// The carried sections as the summary's tail: each quote verbatim with the message it is
    /// from, the standing rules the work touched as pointers, and the identifiers with where they
    /// were last seen.
    pub fn render(&self, session: &str) -> String {
        let mut out = String::new();
        for section in Section::ALL {
            let items: Vec<&Item> = self.items.iter().filter(|i| i.section == section).collect();
            out.push_str(&format!("\n\n## {} (verbatim, carried)\n", section.heading()));
            if items.is_empty() {
                out.push_str("\nnone\n");
            }
            for item in items {
                out.push_str(&format!("\n- [m:{}] {}", item.m, item.quote.trim()));
            }
            out.push('\n');
        }
        if !self.pointers.is_empty() {
            out.push_str(
                "\n\n## Standing rules this work touched (pointers: look each id up where your rules \
                 live; their text is not copied, and rules the harness re-injects are not listed)\n\n",
            );
            for p in &self.pointers {
                out.push_str(&format!("- `{}` ({})\n", p.id, p.why));
            }
        }
        if !self.lines.is_empty() {
            out.push_str(
                "\n\n## What the work stated (lines copied whole, by shape: each holds a sha, a row or a count; \
                 not verified now: probe before relying on one)\n\n",
            );
            for l in &self.lines {
                out.push_str(&format!("- [m:{}] {}\n", l.m, l.line));
            }
        }
        if !self.ids.is_empty() {
            out.push_str(&format!(
                "\n\n## Live identifiers (seen in the transcript; not verified now: probe before claiming one \
                 exists or is absent; `cv show {session} --around ID` opens where each was last seen)\n\n"
            ));
            for id in &self.ids {
                out.push_str(&format!("- `{}` [m:{}]\n", id.id, id.last));
            }
        }
        out
    }

    /// The open promises.
    pub fn promises(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| i.section == Section::Promise)
    }

    /// The carried items as the extraction pass is shown them, by section, each on one line: what
    /// it marks done, used or superseded, quoting the line.
    pub fn listing(&self) -> String {
        let mut out = String::new();
        for section in Section::ALL {
            let mut items = self.items.iter().filter(|i| i.section == section).peekable();
            if items.peek().is_none() {
                continue;
            }
            out.push_str(&format!("{}:\n", section.name()));
            for item in items {
                let mut line = String::new();
                super::anchor::normalize_into(&mut line, &item.quote);
                out.push_str(&format!("- {line}\n"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        ids: 10,
        pointers: 10,
        retire_after: 3,
        standing_bytes: 64,
        lines_bytes: 256,
    };

    fn turn(role: &'static str, m: &str, text: &str) -> (&'static str, String, String) {
        (role, m.to_owned(), text.to_owned())
    }

    #[test]
    fn lines_keep_the_shas_and_counts_the_agent_stated_by_shape() {
        let found = lines(
            &[
                turn("user", "p", "land 0e4c8385a9 please, 130 tests"),
                turn(
                    "assistant",
                    "a",
                    "Landed 0e4c8385a9 with 130 tests passing.\nNothing else here.\n[tool call: Bash {\"command\":\"chat post 'held issue/204 open'\\nls\"}]",
                ),
                turn("user", "r", "[tool result] 2 of 9 modules red\ndone"),
            ],
            4,
        );
        let got: Vec<(&str, &str, Source)> = found.iter().map(|l| (l.m.as_str(), l.line.as_str(), l.from)).collect();
        assert_eq!(
            got,
            [
                ("a", "Landed 0e4c8385a9 with 130 tests passing.", Source::Said),
                ("a", "chat post 'held issue/204 open'", Source::Ran),
                ("r", "2 of 9 modules red", Source::Read),
            ]
        );
        assert!(found.iter().all(|l| l.window == 4));
    }

    #[test]
    fn lines_refuse_bare_hex_output_shapes_and_dumps() {
        let long = format!("{} 12 tests", "x".repeat(LINE_MAX));
        let found = lines(
            &[turn(
                "assistant",
                "a",
                &format!("deadbeef is a word here\n{long}\n2026-10-31T09:10:32Z unit[12]: 4 tests\n1234567"),
            )],
            0,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_tight_lines_budget_keeps_this_window_then_the_agents_own_words() {
        let line = |line: &str, from, window| Line {
            line: line.to_owned(),
            m: "m".to_owned(),
            from,
            window,
        };
        let cost = |s: &str| s.len() + 1 + 8;
        let kept = within_lines_budget(
            vec![
                line("old said 1 of 2", Source::Said, 0),
                line("new read 3 of 4", Source::Read, 1),
                line("new said 5 of 6", Source::Said, 1),
                line("new said 7 of 8", Source::Said, 1),
            ],
            cost("new read 3 of 4") * 2,
            1,
        );
        let got: Vec<&str> = kept.iter().map(|l| l.line.as_str()).collect();
        assert_eq!(got, ["new said 5 of 6", "new said 7 of 8"]);
    }

    #[test]
    fn a_line_said_again_moves_to_its_newest_place_and_a_merge_stays_in_budget() {
        let line = |s: &str, m: &str, window| Line {
            line: s.to_owned(),
            m: m.to_owned(),
            from: Source::Said,
            window,
        };
        let first = Carry::default().merge(
            Span {
                lines: vec![line("a 1 of 2", "m1", 0), line("b 3 of 4", "m2", 0)],
                ..Span::default()
            },
            &LIMITS,
        );
        let next = first.merge(
            Span {
                lines: vec![line("a 1 of 2", "m3", 1)],
                window: 1,
                ..Span::default()
            },
            &LIMITS,
        );
        let got: Vec<(&str, &str)> = next.lines.iter().map(|l| (l.line.as_str(), l.m.as_str())).collect();
        assert_eq!(got, [("b 3 of 4", "m2"), ("a 1 of 2", "m3")]);
        assert!(next.render("s").contains("- [m:m3] a 1 of 2\n"));
        let tight = next.merge(
            Span::default(),
            &Limits {
                lines_bytes: 0,
                ..LIMITS
            },
        );
        assert!(tight.lines.is_empty());
    }

    fn item(section: Section, quote: &str, m: &str) -> Item {
        Item {
            section,
            quote: quote.into(),
            m: m.into(),
            why: String::new(),
            seen: None,
        }
    }

    /// A line of the harness's own summary is carried only from that summary, verbatim; a log-shaped
    /// line is fine there, since the harness chose to keep it.
    #[test]
    fn a_standing_line_quotes_only_the_harness_summary() {
        let texts: HashMap<String, String> = [
            (
                "s1".into(),
                "## Goal\n2026-10-29T07:44 the cron runs hourly\n".to_owned(),
            ),
            ("u1".into(), "the cron runs hourly".to_owned()),
        ]
        .into();
        let layers = Layers {
            prompts: ["u1".into()].into(),
            summaries: ["s1".into()].into(),
            ..Default::default()
        };
        let mut dropped = Dropped::default();
        let kept = verify(
            vec![
                item(Section::Standing, "2026-10-29T07:44 the cron runs hourly", "s1"),
                item(Section::Standing, "the cron runs hourly", "u1"),
                item(Section::Standing, "the cron runs daily", "s1"),
            ],
            &texts,
            &layers,
            &mut dropped,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].m, "s1");
        assert_eq!((dropped.outside_layer, dropped.unverified), (1, 1));
    }

    /// The standing lines stay within their budget, the newest kept, and retire like any item.
    #[test]
    fn standing_lines_keep_the_newest_within_their_budget_and_retire_unused() {
        let line = |c: char| c.to_string().repeat(40);
        let at = |window, items| Span {
            items,
            window,
            ..Default::default()
        };
        let first = Carry::default().merge(at(1, vec![item(Section::Standing, &line('a'), "s1")]), &LIMITS);
        let second = first.merge(at(2, vec![item(Section::Standing, &line('b'), "s2")]), &LIMITS);
        let quotes = |c: &Carry| c.items.iter().map(|i| i.quote.clone()).collect::<Vec<_>>();
        assert_eq!(quotes(&second), vec![line('b')], "64 bytes hold one line, the newer");
        let wide = Limits {
            standing_bytes: 1024,
            ..LIMITS
        };
        let both = first.merge(at(2, vec![item(Section::Standing, &line('b'), "s2")]), &wide);
        assert_eq!(quotes(&both), vec![line('a'), line('b')]);
        assert!(both.merge(at(5, Vec::new()), &wide).items.iter().all(|i| i.m == "s2"));
        assert!(both.render("s").contains("as of that summary"));
    }

    /// An item retires after `retire_after` windows in which no span said or used it; a use
    /// (marked by the extraction pass, or the quote said again) restarts the count. A ruling
    /// never retires by age: only a later ruling of the person's supersedes it.
    #[test]
    fn unused_items_retire_and_rulings_leave_only_when_superseded() {
        let at = |window, items, marks| Span {
            items,
            marks,
            window,
            ..Default::default()
        };
        let carry = Carry::default().merge(
            at(
                10,
                vec![
                    item(Section::Rule, "never push main", "u1"),
                    item(Section::Term, "a lane is a worktree", "u1"),
                    item(Section::Fact, "the seat runs opus", "t1"),
                ],
                Marks::default(),
            ),
            &LIMITS,
        );
        assert!(carry.items.iter().all(|i| i.seen == Some(10)));
        // Window 12: the term is used again; the fact is not.
        let used = Marks {
            used: vec!["a lane is a worktree".into()],
            ..Default::default()
        };
        let carry = carry.merge(at(12, Vec::new(), used), &LIMITS);
        assert_eq!(carry.items.len(), 3);
        // Window 13: three windows since the fact was last said, so it retires; the term was
        // used in 12, the ruling never retires by age.
        let carry = carry.merge(at(13, Vec::new(), Marks::default()), &LIMITS);
        let quotes: Vec<&str> = carry.items.iter().map(|i| i.quote.as_str()).collect();
        assert_eq!(quotes, ["never push main", "a lane is a worktree"]);
        let carry = carry.merge(at(40, Vec::new(), Marks::default()), &LIMITS);
        assert_eq!(carry.items.len(), 1, "the ruling outlives any age");
        // A later ruling supersedes it; the new one is carried in its place.
        let superseded = Marks {
            superseded: vec!["never push main".into()],
            ..Default::default()
        };
        let carry = carry.merge(
            at(
                41,
                vec![item(Section::Rule, "push main after the review", "u9")],
                superseded,
            ),
            &LIMITS,
        );
        let quotes: Vec<&str> = carry.items.iter().map(|i| i.quote.as_str()).collect();
        assert_eq!(quotes, ["push main after the review"]);
        // A record written before the count: its items start counting from this window.
        let mut old = Carry::default();
        old.items.push(item(Section::Fact, "the host is alpha", "t2"));
        let carry = old.merge(at(50, Vec::new(), Marks::default()), &LIMITS);
        assert_eq!(carry.items[0].seen, Some(50));
    }

    /// A mark counts only when it names a carried item and cites a message that can make it:
    /// a supersession is the person's own prompt, a use or fulfilment a message of the span.
    #[test]
    fn marks_name_carried_items_from_messages_that_can_make_them() {
        let carried = Carry {
            items: vec![
                item(Section::Rule, "never push main", "u1"),
                item(Section::Promise, "I will run the tests", "a1"),
                item(Section::Fact, "the seat runs opus", "t1"),
            ],
            ..Default::default()
        };
        let texts = HashMap::from([
            ("u9".to_owned(), "push main after the review".to_owned()),
            ("a9".to_owned(), "tests pass; the seat   runs opus".to_owned()),
        ]);
        let layers = Layers {
            prompts: ["u9".to_owned()].into(),
            ..Default::default()
        };
        let mark = |kind, quote: &str, m: &str| Mark {
            kind,
            quote: quote.into(),
            m: m.into(),
        };
        let mut dropped = Dropped::default();
        let marks = verify_marks(
            vec![
                mark(MarkKind::Superseded, "never  push main", "u9"),
                mark(MarkKind::Superseded, "never push main", "a9"),
                mark(MarkKind::Done, "I will run the tests", "a9"),
                mark(MarkKind::Done, "the seat runs opus", "a9"),
                mark(MarkKind::Used, "the seat runs opus", "zz"),
                mark(MarkKind::Used, "something never carried", "a9"),
            ],
            &carried,
            &texts,
            &layers,
            &mut dropped,
        );
        assert_eq!(marks.superseded, ["never push main"]);
        assert_eq!(marks.done, ["I will run the tests"]);
        assert!(marks.used.is_empty());
        assert_eq!(dropped.marks, 4);
        let pairs: Vec<(String, String)> = texts.into_iter().collect();
        assert_eq!(said_again(&carried, &pairs), ["the seat runs opus"]);
        let listing = carried.listing();
        assert!(listing.contains("rule:\n- never push main\n"), "{listing}");
        assert!(listing.contains("promise:\n- I will run the tests\n"), "{listing}");
    }

    /// Output lines are not facts: a leading timestamp, a `unit[pid]:` tag or a one-line commit
    /// marks a quote as noise; a statement with a date inside it is not.
    #[test]
    fn output_lines_are_noise_and_statements_are_not() {
        for line in [
            "Thu 2026-10-29 00:05:00 PDT 49min - meta-kimi-window-wake.timer",
            "2026-10-29T07:44:01Z precompact: wrote",
            "Oct 29 07:44:01 hub systemd[1]: Started fab.service.",
            "07:44:01 build finished",
            "hub fleet-bus[2211]: seat woke",
            "13439a7d13 fix(autoswitch): retarget decodes the leading --json reading",
        ] {
            assert!(noise(line), "{line}");
        }
        for line in [
            "the seat has a 1M context",
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW=400000",
            "the land fix shipped on 2026-10-23",
            "I am the lead seat.",
        ] {
            assert!(!noise(line), "{line}");
        }
        let texts = HashMap::from([(
            "t1".to_owned(),
            "Thu 2026-10-29 00:05:00 PDT 49min - x.timer".to_owned(),
        )]);
        let mut dropped = Dropped::default();
        let kept = verify(
            vec![item(Section::Fact, "Thu 2026-10-29 00:05:00 PDT 49min - x.timer", "t1")],
            &texts,
            &Layers::default(),
            &mut dropped,
        );
        assert!(kept.is_empty());
        assert_eq!(dropped.noise, 1);
    }

    /// Pointers run from least to most recently used, and only the newest `pointers` are kept.
    #[test]
    fn pointers_keep_the_most_recently_used() {
        let found: Vec<(String, String)> = [("a1", "x-a"), ("a2", "x-b"), ("a3", "x-a"), ("a4", "x-c")]
            .iter()
            .map(|(m, id)| ((*m).into(), (*id).into()))
            .collect();
        let made = pointers(&found);
        let names: Vec<&str> = made.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(names, ["x-b", "x-a", "x-c"], "x-a was used again after x-b");
        assert_eq!(made[1].hits, 2);
        let first = Carry::default().merge(
            Span {
                pointers: made,
                ..Default::default()
            },
            &Limits { pointers: 2, ..LIMITS },
        );
        let names: Vec<&str> = first.pointers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(names, ["x-a", "x-c"]);
        let second = first.merge(
            Span {
                pointers: pointers(&[("a9".into(), "x-a".into())]),
                ..Default::default()
            },
            &Limits { pointers: 2, ..LIMITS },
        );
        let names: Vec<&str> = second.pointers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(names, ["x-c", "x-a"], "a pointer used again moves to the newest end");
    }

    /// Every cite names a message of the window in full: a unique prefix of 8 or more characters
    /// is completed, and a cite of no such message is removed and counted.
    #[test]
    fn cites_resolve_to_window_messages_in_full_or_are_removed() {
        let known: std::collections::HashSet<String> = [
            "4d1dd21f-7a00-4c1e-9d1f-000000000001",
            "4d1dd21f-7a00-4c1e-9d1f-000000000002",
            "f9f71650-1111-4222-8333-444444444444",
            "u1",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let (text, counts) = cites(
            "ruled [m:u1]; kept [m:f9f71650]; both [m:4d1dd21f]; made up [m:991de80a-0000]; \
             full [m:4d1dd21f-7a00-4c1e-9d1f-000000000002]; short [m:f9f7]",
            &known,
        );
        assert_eq!(
            text,
            "ruled [m:u1]; kept [m:f9f71650-1111-4222-8333-444444444444]; both ; made up ; \
             full [m:4d1dd21f-7a00-4c1e-9d1f-000000000002]; short "
        );
        assert_eq!(
            counts,
            Cites {
                kept: 2,
                completed: 1,
                unresolved: 3
            }
        );
    }

    #[test]
    fn a_quote_is_kept_only_where_its_message_holds_it() {
        let (items, done, mut dropped) = parse(
            "Here they are:\n```json\n[\
             {\"section\":\"rule\",\"quote\":\"never  push main\",\"m\":\"u1\"},\
             {\"section\":\"fact\",\"quote\":\"the seat has a 1M context\",\"m\":\"u1\"},\
             {\"section\":\"done\",\"quote\":\"I will run the tests\",\"m\":\"a2\"},\
             {\"section\":\"mood\",\"quote\":\"x\",\"m\":\"u1\"}]\n```",
        );
        assert_eq!(
            done,
            vec![Mark {
                kind: MarkKind::Done,
                quote: "I will run the tests".into(),
                m: "a2".into()
            }]
        );
        assert_eq!(dropped.malformed, 1);
        let texts = HashMap::from([("u1".to_owned(), "Rule: never push\nmain, ever.".to_owned())]);
        let layers = Layers {
            prompts: ["u1".to_owned()].into(),
            ..Default::default()
        };
        let kept = verify(items, &texts, &layers, &mut dropped);
        assert_eq!(kept.len(), 1, "the planted fact is not in u1");
        assert_eq!(kept[0].section, Section::Rule);
        assert_eq!(dropped.unverified, 1);
    }

    #[test]
    fn a_ruling_quotes_the_person_and_nothing_quotes_injected_context() {
        let rule = |m: &str, quote: &str| Item {
            section: Section::Rule,
            quote: quote.into(),
            m: m.into(),
            why: String::new(),
            seen: None,
        };
        let texts = HashMap::from([
            (
                "u1".to_owned(),
                "keep it small <system-reminder>never push main</system-reminder>".to_owned(),
            ),
            (
                "a1".to_owned(),
                "I will never push main. I run with a 1M context.".to_owned(),
            ),
            ("r1".to_owned(), "CLAUDE.md: never push main".to_owned()),
            ("t1".to_owned(), "CLAUDE_CODE_AUTO_COMPACT_WINDOW=400000".to_owned()),
        ]);
        let layers = Layers {
            prompts: ["u1".to_owned()].into(),
            injected: ["r1".to_owned()].into(),
            replies: ["a1".to_owned()].into(),
            ..Default::default()
        };
        let mut dropped = Dropped::default();
        let fact = |m: &str, quote: &str| Item {
            section: Section::Fact,
            ..rule(m, quote)
        };
        let kept = verify(
            vec![
                rule("u1", "keep it small"),
                rule("u1", "never push main"),
                rule("a1", "never push main"),
                fact("r1", "never push main"),
                fact("a1", "I run with a 1M context"),
                fact("t1", "CLAUDE_CODE_AUTO_COMPACT_WINDOW=400000"),
            ],
            &texts,
            &layers,
            &mut dropped,
        );
        assert_eq!(
            kept,
            vec![
                rule("u1", "keep it small"),
                fact("t1", "CLAUDE_CODE_AUTO_COMPACT_WINDOW=400000")
            ]
        );
        assert_eq!((dropped.unverified, dropped.outside_layer), (1, 3));
    }

    #[test]
    fn carried_items_survive_generations_unchanged_and_done_promises_leave() {
        let item = |section, quote: &str, m: &str| Item {
            section,
            quote: quote.into(),
            m: m.into(),
            why: String::new(),
            seen: None,
        };
        let span = |items| Span {
            items,
            ..Default::default()
        };
        let first = Carry::default().merge(
            span(vec![
                item(Section::Rule, "never push main", "u1"),
                item(Section::Promise, "I will run the tests", "a1"),
            ]),
            &LIMITS,
        );
        assert_eq!(first.items[0].why, "said in this span at [m:u1]");
        let second = first.merge(
            Span {
                items: vec![
                    item(Section::Rule, "never  push main", "u9"),
                    item(Section::Term, "a lane is a worktree", "u3"),
                ],
                pointers: pointers(&[
                    ("a4".into(), "lane-commits".into()),
                    ("a5".into(), "lane-commits".into()),
                ]),
                marks: Marks {
                    done: vec!["I will run the tests".to_owned()],
                    ..Default::default()
                },
                ..Default::default()
            },
            &LIMITS,
        );
        assert_eq!(second.items.len(), 2);
        assert_eq!(
            (second.items[0].quote.as_str(), second.items[0].m.as_str()),
            ("never push main", "u1"),
            "the first wording stands"
        );
        assert!(second.items[0].why.starts_with("carried from an earlier summary"));
        assert!(second.promises().next().is_none());
        assert_eq!(second.pointers[0].hits, 2);
        let text = second.render("s");
        assert!(text.contains("## The person's rulings in force (verbatim, carried)\n\n- [m:u1] never push main"));
        assert!(text.contains("## Open promises (verbatim, carried)\n\nnone"));
        assert!(text.contains("- `lane-commits` (the work in this span touched it 2 time(s), last at [m:a5])"));
    }

    #[test]
    fn identifiers_are_read_by_shape_with_where_they_were_seen() {
        let found = ids(&[
            (
                "a1".into(),
                "committed 19e91340 on task/1234, see /tmp/cv-home/x.log.".into(),
            ),
            (
                "a2".into(),
                "deadbeef and 19e91340 again at https://github.com/akapug/cv/pull/3".into(),
            ),
        ]);
        let names: Vec<&str> = found.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            names,
            [
                "19e91340",
                "task/1234",
                "/tmp/cv-home/x.log",
                "https://github.com/akapug/cv/pull/3"
            ]
        );
        assert_eq!((found[0].first.as_str(), found[0].last.as_str()), ("a1", "a2"));
        let carry = Carry::default().merge(
            Span {
                ids: found,
                ..Default::default()
            },
            &Limits { ids: 2, ..LIMITS },
        );
        assert_eq!(carry.ids.len(), 2, "the newest kept");
        assert!(carry
            .render("s")
            .contains("probe before claiming one exists or is absent"));
    }
}
