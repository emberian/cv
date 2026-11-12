//! Bounded reductions over complete decoded content, shared by affordance counts and rows.
use crate::ir::Harness;
use crate::lazy::{Resolver, Text};
use anyhow::Result;

const CHUNK: usize = 64 * 1024;
const SUMMARY_CHARS: usize = 120;

/// A summary is the trimmed [`Head`](crate::projection::Head) of `SUMMARY_CHARS`: exactly
/// `truncate(trim(text), 120)`, a long whitespace tail never turning it into an ellipsis.
type Trimmed = crate::projection::Head;
fn trimmed() -> Trimmed {
    crate::projection::Head::trimmed(SUMMARY_CHARS)
}

/// The longest suffix that could continue an ASCII delimiter in the next decoded chunk.
fn partial_suffix(text: &str, delimiter: &str) -> usize {
    (1..delimiter.len())
        .rev()
        .find(|&n| text.ends_with(&delimiter[..n]))
        .unwrap_or(0)
}

struct Tag {
    open: &'static str,
    close: &'static str,
    opened: bool,
    complete: bool,
    pending: String,
    scratch: String,
    text: Trimmed,
}

impl Tag {
    fn new(open: &'static str, close: &'static str) -> Self {
        Self {
            open,
            close,
            opened: false,
            complete: false,
            pending: String::new(),
            scratch: String::new(),
            text: trimmed(),
        }
    }

    fn push(&mut self, chunk: &str) {
        if self.complete {
            return;
        }
        self.scratch.clear();
        self.scratch.push_str(&self.pending);
        self.scratch.push_str(chunk);
        self.pending.clear();
        let mut body = self.scratch.as_str();
        if !self.opened {
            if let Some(start) = body.find(self.open) {
                self.opened = true;
                body = &body[start + self.open.len()..];
            } else {
                let n = partial_suffix(body, self.open);
                self.pending.push_str(&body[body.len() - n..]);
                return;
            }
        }
        if let Some(end) = body.find(self.close) {
            self.text.push(&body[..end]);
            self.complete = true;
        } else {
            let n = partial_suffix(body, self.close);
            self.text.push(&body[..body.len() - n]);
            self.pending.push_str(&body[body.len() - n..]);
        }
    }
}

struct Contains {
    needle: &'static str,
    found: bool,
    pending: String,
}

impl Contains {
    fn new(needle: &'static str) -> Self {
        Self {
            needle,
            found: false,
            pending: String::new(),
        }
    }

    fn push(&mut self, chunk: &str) {
        if self.found {
            return;
        }
        if chunk.contains(self.needle) {
            self.found = true;
            return;
        }
        let mut end = chunk.len().min(self.needle.len());
        while !chunk.is_char_boundary(end) {
            end += 1;
        }
        let mut bridge = std::mem::take(&mut self.pending);
        bridge.push_str(&chunk[..end]);
        if bridge.contains(self.needle) {
            self.found = true;
            return;
        }
        let tail = if end == chunk.len() { bridge.as_str() } else { chunk };
        let n = partial_suffix(tail, self.needle);
        self.pending.push_str(&tail[tail.len() - n..]);
    }
}

pub(super) struct Facts {
    plain: Trimmed,
    summary: Tag,
    reminder: Tag,
}

impl Default for Facts {
    fn default() -> Self {
        Self {
            plain: trimmed(),
            summary: Tag::new("<summary>", "</summary>"),
            reminder: Tag::new("<system-reminder>", "</system-reminder>"),
        }
    }
}

impl Facts {
    fn push(&mut self, chunk: &str) {
        self.plain.push(chunk);
        self.summary.push(chunk);
        self.reminder.push(chunk);
    }

    pub(super) fn summary(&self) -> String {
        if self.summary.complete {
            self.summary.text.finish()
        } else if self.reminder.complete {
            self.reminder.text.finish()
        } else {
            self.plain.finish()
        }
    }
}

fn chunks(text: &Text, resolver: &Resolver, mut visit: impl FnMut(&str)) -> Result<()> {
    text.try_for_each_chunk(resolver, CHUNK, |part| {
        visit(part);
        Ok(())
    })
}

// Checked readers validate the captured source even when the callback stops traversal.
fn until(text: &Text, resolver: &Resolver, mut done: impl FnMut(&str) -> bool) -> Result<()> {
    match text.try_for_each_chunk(resolver, CHUNK, |part| {
        if done(part) {
            Err(crate::stream::Stop.into())
        } else {
            Ok(())
        }
    }) {
        Err(error) if error.is::<crate::stream::Stop>() => Ok(()),
        result => result,
    }
}

pub(super) fn facts(text: &Text, resolver: &Resolver) -> Result<Facts> {
    let mut facts = Facts::default();
    chunks(text, resolver, |chunk| facts.push(chunk))?;
    Ok(facts)
}

/// Most record heads decide from a small checked source prefix. Only a long whitespace run or
/// an incomplete decoded prefix needs the chunk path; it retains only the wanted ASCII head.
pub(super) fn starts_with(text: &Text, resolver: &Resolver, wanted: &str) -> Result<bool> {
    if let Text::Inline(text) = text {
        return Ok(text.trim_start().starts_with(wanted));
    }
    let mut head = String::with_capacity(wanted.len());
    let mut remaining = wanted.chars().count();
    let mut started = false;
    until(text, resolver, |mut chunk| {
        if remaining == 0 {
            return true;
        }
        if !started {
            chunk = chunk.trim_start();
            started = !chunk.is_empty();
        }
        for ch in chunk.chars().take(remaining) {
            head.push(ch);
            remaining -= 1;
        }
        remaining == 0
    })?;
    Ok(head.starts_with(wanted))
}

pub(super) fn contains(text: &Text, resolver: &Resolver, needle: &'static str) -> Result<bool> {
    if let Text::Inline(text) = text {
        return Ok(text.contains(needle));
    }
    let mut search = Contains::new(needle);
    until(text, resolver, |chunk| {
        search.push(chunk);
        search.found
    })?;
    Ok(search.found)
}

/// Whether an error result reads as one of `harness`'s denials
/// ([`crate::harness::Capabilities::denials`]), tried in order. Prefix and `also` needle are
/// decided in one traversal: the head accumulates only the wanted prefix characters while a
/// [`Contains`] streams the needle search.
pub(super) fn denied(text: &Text, resolver: &Resolver, harness: Harness) -> Result<bool> {
    let denials = crate::harness::rootless(harness).map_or(&[][..], |adapter| adapter.capabilities().denials);
    for denial in denials {
        let mut head = String::with_capacity(denial.prefix.len());
        let mut remaining = denial.prefix.chars().count();
        let mut started = false;
        let mut search = denial.also.map(Contains::new);
        let mut matched = false;
        until(text, resolver, |mut chunk| {
            if let Some(search) = search.as_mut() {
                search.push(chunk);
            }
            if remaining > 0 {
                if !started {
                    chunk = chunk.trim_start();
                    started = !chunk.is_empty();
                }
                for ch in chunk.chars().take(remaining) {
                    head.push(ch);
                    remaining -= 1;
                }
                if remaining > 0 {
                    return false;
                }
                matched = head.starts_with(denial.prefix);
                if !matched {
                    return true;
                }
            }
            search.as_ref().is_none_or(|search| search.found)
        })?;
        if matched && search.as_ref().is_none_or(|search| search.found) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn plain_summary(text: &Text, resolver: &Resolver) -> Result<String> {
    let mut summary = trimmed();
    chunks(text, resolver, |chunk| {
        summary.push(chunk);
    })?;
    Ok(summary.finish())
}

/// Detect a Codex Code Mode `exec` / `wait` result that yielded a background V8 cell
/// (`"Script running with cell ID <id>\n..."`). Streams only the first line.
pub(super) fn running_cell_summary(
    text: &Text,
    resolver: &Resolver,
    harness: Harness,
    summaries: bool,
) -> Result<Option<String>> {
    match harness {
        Harness::Codex => {}
        Harness::Claude
        | Harness::Grok
        | Harness::OpenCode
        | Harness::Gemini
        | Harness::Antigravity
        | Harness::Hermes
        | Harness::OpenClaw
        | Harness::Cursor
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Qwen
        | Harness::LmStudio
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => return Ok(None),
    }
    const PREFIX: &str = "Script running with cell ID ";
    let mut line = String::new();
    let mut started = false;
    until(text, resolver, |mut chunk| {
        if !started {
            chunk = chunk.trim_start();
            started = !chunk.is_empty();
        }
        for ch in chunk.chars() {
            if ch == '\n' || ch == '\r' || line.len() >= 120 {
                return true;
            }
            line.push(ch);
        }
        false
    })?;
    if !line.starts_with(PREFIX) {
        return Ok(None);
    }
    Ok(Some(if summaries {
        crate::ir::truncate(line.trim(), SUMMARY_CHARS)
    } else {
        String::new()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_source_reductions_match_inline_and_fail_after_early_stop_mutation() {
        let world = crate::test_world("affordance-source-reductions");
        let path = world.base.join("source.json");
        let body = format!(
            "{}Permission to use Bash {} denied<summary>α🌙\nsummary</summary>",
            "\u{2003}".repeat(30000),
            "x".repeat(CHUNK - 4)
        );
        let wire = serde_json::to_vec(&body).unwrap();
        std::fs::write(&path, &wire).unwrap();
        let source = crate::Json::source(&path, 0, wire.len() as u64)
            .unwrap()
            .with_view(|view| view.source_text())
            .unwrap()
            .unwrap();
        let inline = Text::Inline(body.clone());
        let resolver = Resolver::new(None);
        assert_eq!(facts(&source, &resolver).unwrap().summary(), original_summary(&body));
        assert_eq!(
            plain_summary(&source, &resolver).unwrap(),
            plain_summary(&inline, &resolver).unwrap()
        );
        assert!(starts_with(&source, &resolver, "Permission to use ").unwrap());
        assert!(contains(&source, &resolver, " denied").unwrap());
        assert!(denied(&source, &resolver, Harness::Claude).unwrap());
        let mut visits = 0;
        until(&source, &resolver, |_| {
            visits += 1;
            true
        })
        .unwrap();
        assert_eq!(visits, 1);
        let error = until(&source, &resolver, |_| {
            std::fs::write(&path, "mutated").unwrap();
            true
        })
        .unwrap_err();
        assert!(error.to_string().contains("changed"), "{error:#}");
        assert!(facts(&source, &resolver).is_err());
        assert!(starts_with(&source, &resolver, "absent").is_err());
        assert!(contains(&source, &resolver, " denied").is_err());
        assert!(denied(&source, &resolver, Harness::Claude).is_err());
    }

    fn original_summary(text: &str) -> String {
        let inner = |tag: &str| {
            let open = format!("<{tag}>");
            let start = text.find(&open)? + open.len();
            let end = text[start..].find(&format!("</{tag}>"))? + start;
            Some(&text[start..end])
        };
        crate::ir::truncate(
            inner("summary")
                .or_else(|| inner("system-reminder"))
                .unwrap_or(text)
                .trim(),
            120,
        )
    }

    #[test]
    fn reductions_match_full_text_across_every_chunk_boundary() {
        let examples = [
            String::new(),
            format!("{}late first text{}", "\u{2003}".repeat(8200), "\n".repeat(9000)),
            format!("{}{}", "x".repeat(120), " \n\t".repeat(9000)),
            format!("{}{}z", "x".repeat(120), " \n\t".repeat(9000)),
            format!(
                "{}<summary>{}late summary{}</summary>",
                "x".repeat(9000),
                "\n".repeat(8200),
                "\u{2003}".repeat(9000)
            ),
            "<system-reminder>fallback</system-reminder><summary>first<summary>second</summary>ignored</summary>"
                .into(),
            "<summary>unclosed <system-reminder>fallback</system-reminder>".into(),
            "<summary></summary><summary>second</summary>".into(),
            format!(
                "{}Permission to use Bash {} denied{}",
                " \n".repeat(9000),
                "x".repeat(9000),
                "\n".repeat(9000)
            ),
            format!(
                "{}The user rejected permission to use this specific tool call",
                "\u{2003}".repeat(9000)
            ),
            format!("{}requires approval", "x".repeat(9000)),
            "😀</summary><summar<summary>α\nβ😀</summary><summary>ignored</summary>".into(),
        ];
        for text in examples {
            for chunk in [1, 2, 3, 7, 13, 63, 1024, 4096, 65536] {
                let mut facts = Facts::default();
                let mut rest = text.as_str();
                while !rest.is_empty() {
                    let mut end = chunk.min(rest.len());
                    while !rest.is_char_boundary(end) {
                        end += 1;
                    }
                    facts.push(&rest[..end]);
                    rest = &rest[end..];
                }
                assert_eq!(facts.summary(), original_summary(&text), "chunk={chunk}, text={text:?}");
                assert_eq!(facts.plain.finish(), crate::ir::truncate(text.trim(), 120));
                let (whole, resolver) = (Text::Inline(text.clone()), Resolver::new(None));
                assert_eq!(
                    contains(&whole, &resolver, "requires approval").unwrap(),
                    text.trim_start().contains("requires approval")
                );
                let t = text.trim_start();
                assert_eq!(
                    denied(&whole, &resolver, Harness::Claude).unwrap(),
                    (t.starts_with("Permission to use ") && t.contains(" denied"))
                        || t.starts_with("The user doesn't want to proceed with this tool use")
                );
                assert_eq!(
                    denied(&whole, &resolver, Harness::OpenCode).unwrap(),
                    t.starts_with(
                        "The user has specified a rule which prevents you from using this specific tool call"
                    ) || t.starts_with("The user rejected permission to use this specific tool call")
                );
            }
        }
    }
    #[test]
    fn escaped_and_literal_spans_match_full_inline_reductions() {
        let dir = std::env::temp_dir().join(format!("cv-affordance-text-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let examples = [
            format!("{}<task-notification>requires approval", "\u{2003}".repeat(100)),
            format!(
                "{}<summary>{}α\nβ😀{}</summary>",
                "p".repeat(70000),
                "\u{2003}".repeat(70000),
                "\n".repeat(70000)
            ),
            format!(
                "{}Permission to use Bash {} denied{}",
                "\t".repeat(70000),
                "x".repeat(70000),
                "\n".repeat(70000)
            ),
            format!(
                "{}<task-notification>{}requires approval",
                "\n".repeat(70000),
                "x".repeat(70000)
            ),
            format!("{}{}", "😀".repeat(120), "\u{2003}".repeat(70000)),
        ];
        for (i, body) in examples.iter().enumerate() {
            for escaped in [false, true] {
                let path = dir.join(format!("{i}-{escaped}"));
                // A span covers a JSON string's body; `escaped` is only the body's own flag.
                let raw = serde_json::to_string(body).unwrap();
                std::fs::write(&path, &raw).unwrap();
                let resolver = Resolver::new(Some(path));
                let span = Text::Span(crate::lazy::Span {
                    source: None,
                    offset: 1,
                    len: raw.len() as u64 - 2,
                    escaped,
                });
                let reduced = facts(&span, &resolver).unwrap();
                assert_eq!(reduced.summary(), original_summary(body));
                assert_eq!(reduced.plain.finish(), crate::ir::truncate(body.trim(), 120));
                assert_eq!(
                    contains(&span, &resolver, "requires approval").unwrap(),
                    body.contains("requires approval")
                );
                let full = Text::Inline(body.clone());
                assert_eq!(
                    denied(&span, &resolver, Harness::Claude).unwrap(),
                    denied(&full, &resolver, Harness::Claude).unwrap()
                );
                assert_eq!(
                    starts_with(&span, &resolver, "<task-notification>").unwrap(),
                    body.trim_start().starts_with("<task-notification>")
                );
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn unicode_whitespace_across_prefix_cap_keeps_the_late_ascii_head() {
        let dir = std::env::temp_dir().join(format!("cv-affordance-unicode-head-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let full = format!(
            "{}<task-notification>wanted</task-notification>",
            "\u{2003}".repeat(10000)
        );
        let path = dir.join("body");
        std::fs::write(&path, format!("\"{full}\"")).unwrap();
        let resolver = Resolver::new(Some(path));
        for escaped in [false, true] {
            let span = Text::Span(crate::lazy::Span {
                source: None,
                offset: 1,
                len: full.len() as u64,
                escaped,
            });
            assert!(span.try_resolve_prefix(&resolver, 256).unwrap().trim().is_empty());
            assert_eq!(
                starts_with(&span, &resolver, "<task-notification>").unwrap(),
                full.trim_start().starts_with("<task-notification>")
            );
        }
        drop(resolver);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
