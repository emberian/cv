//! Shared terminal, CommonMark fence, and role-marker sanitization for untrusted text.
//!
//! 1. **Terminal rows** ([`sanitize_line`], [`strip_ansi_into`]): strip ESC/C1 control sequences and
//!    Unicode Bidi (`Cf`) Trojan-Source codepoints (`U+202A..=U+202E`, `U+2066..=U+2069`).
//! 2. **Markdown / prompt views** ([`backtick_fence`], [`tilde_fence`], [`fenced_block`],
//!    [`fenced_block_indented`], [`inline_code_span`], [`unclosed_fence`], [`close_unclosed_fence`],
//!    [`neutralize_forged_headers`], [`neutralize_non_human`], [`write_neutralized_chunks`]): prevent
//!    fence breakouts and forged `[User]:` / `# USER` / section boundary spoofing across `render`,
//!    `distill`, `pack`, `overview`, and `workflow`.

use crate::ir::Origin;
use std::borrow::Cow;
use std::io::Write;

/// Whether `c` is a Unicode Bidirectional Format (`Cf`) Trojan-Source spoofing codepoint:
/// directional embeddings/overrides/PDF (`U+202A..=U+202E`) and directional isolates (`U+2066..=U+2069`).
#[inline]
pub fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

#[inline]
fn needs_sanitizing(c: char) -> bool {
    c.is_control() || is_bidi_control(c)
}

/// Make `s` safe for a one-line terminal row: ESC/C1-initiated sequences (CSI, OSC, DCS, SOS, PM,
/// APC) are dropped entirely (parameters and all), each `\n`/`\t`/`\r` becomes one space (render
/// sites are one-line rows), and every other control character (C0, DEL, C1) or Unicode Bidi (`Cf`)
/// spoofing codepoint (`U+202A..=U+202E`, `U+2066..=U+2069`) is removed.
/// Returns the input borrowed when it is already clean — the overwhelmingly common case.
pub fn sanitize_line(s: &str) -> Cow<'_, str> {
    if !s.chars().any(needs_sanitizing) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    sanitize_into(s, true, &mut out);
    Cow::Owned(out)
}

/// Strip ANSI/ESC/C1 escape sequences, non-structural control characters (C0, DEL, C1, `\r`), and
/// Unicode Bidi Format (`Cf`) Trojan-Source codepoints (`U+202A..=U+202E`, `U+2066..=U+2069`) from
/// `s`, appending the sanitized text into `out` while preserving `\n` and `\t`.
pub fn strip_ansi_into(s: &str, out: &mut String) {
    if !s.chars().any(needs_sanitizing) {
        out.push_str(s);
        return;
    }
    out.reserve(s.len());
    sanitize_into(s, false, out);
}

fn sanitize_into(s: &str, one_line: bool, out: &mut String) {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\t' if !one_line => out.push(c),
            '\n' | '\t' | '\r' if one_line => out.push(' '),
            '\u{1b}' => skip_escape_sequence(&mut chars),
            '\u{9b}' => skip_csi(&mut chars),
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => skip_control_string(&mut chars),
            c if needs_sanitizing(c) => {}
            c => out.push(c),
        }
    }
}

/// Consume the remainder of an ESC-initiated sequence (the ESC itself was already consumed), so
/// the sequence's *payload* (colors, window titles, arbitrary OSC bytes) never reaches the
/// terminal as text.
fn skip_escape_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        // CSI: `ESC [`, then parameter/intermediate bytes until a final byte in `@`..=`~`.
        Some('[') => skip_csi(chars),
        // String-carrying sequences have equivalent ESC and C1 spellings.
        Some(']' | 'P' | 'X' | '^' | '_') => skip_control_string(chars),
        // Character-set designation and other ESC sequences can have intermediate bytes.
        Some(c) if (' '..='/').contains(&c) => {
            while chars.peek().is_some_and(|c| (' '..='/').contains(c)) {
                chars.next();
            }
            if chars.peek().is_some_and(|c| ('0'..='~').contains(c)) {
                chars.next();
            }
        }
        // Two-char sequence (`ESC c`, `ESC 7`, ...) — the second char was just consumed — or a
        // trailing bare ESC (None): nothing more to skip.
        _ => {}
    }
}

fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for c in chars.by_ref() {
        if ('@'..='~').contains(&c) {
            break;
        }
    }
}
fn skip_control_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(c) = chars.next() {
        if c == '\u{07}' || c == '\u{9c}' {
            break;
        }
        if c == '\u{1b}' {
            if chars.peek() == Some(&'\\') {
                chars.next();
            }
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Adaptive CommonMark fences & forged role-marker neutralization
// ---------------------------------------------------------------------------

/// Maximum contiguous run length of `ch` anywhere inside `body`.
pub fn longest_run(body: &str, ch: char) -> usize {
    let mut max_run = 0usize;
    let mut cur = 0usize;
    for c in body.chars() {
        if c == ch {
            cur += 1;
            if cur > max_run {
                max_run = cur;
            }
        } else {
            cur = 0;
        }
    }
    max_run
}

/// A CommonMark code fence of `ch` (`` '`' `` or `'~'`) strictly longer than any contiguous run in `body`.
pub fn fence_for(body: &str, ch: char, min_len: usize) -> String {
    debug_assert!(matches!(ch, '`' | '~'));
    let n = min_len.max(longest_run(body, ch).saturating_add(1));
    std::iter::repeat_n(ch, n).collect()
}

/// Adaptive backtick fence (at least 3 backticks, strictly longer than every backtick run in `body`).
pub fn backtick_fence(body: &str) -> String {
    fence_for(body, '`', 3)
}

/// Adaptive tilde fence (at least 4 tildes, strictly longer than every tilde run in `body`).
pub fn tilde_fence(body: &str) -> String {
    fence_for(body, '~', 4)
}

/// Wrap `body` in an adaptive backtick code block with optional single-line `info` tag.
/// CommonMark forbids backticks in backtick-fence info strings, so any backticks/controls in `info` are stripped.
pub fn fenced_block(body: &str, info: &str) -> String {
    let fence = backtick_fence(body);
    let clean_info = sanitize_line(info);
    let clean_info = clean_info.trim();
    if clean_info.contains('`') {
        let stripped = clean_info.replace('`', "");
        format!("{fence}{stripped}\n{body}\n{fence}")
    } else {
        format!("{fence}{clean_info}\n{body}\n{fence}")
    }
}

/// Like [`fenced_block`], prefixing each fence and body line with `indent` (up to 3 spaces in CommonMark).
pub fn fenced_block_indented(body: &str, info: &str, indent: &str) -> String {
    let fence = backtick_fence(body);
    let clean_info = sanitize_line(info);
    let clean_info = clean_info.trim().replace('`', "");
    let mut out = String::with_capacity(body.len() + fence.len() * 2 + indent.len() * 4 + 8);
    out.push_str(indent);
    out.push_str(&fence);
    out.push_str(&clean_info);
    out.push('\n');
    for line in body.lines() {
        out.push_str(indent);
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(indent);
    out.push_str(&fence);
    out
}

/// Wrap a single-line untrusted label/command/path in an adaptive CommonMark inline code span.
pub fn inline_code_span(s: &str) -> String {
    let clean = sanitize_line(s);
    let ticks = "`".repeat(longest_run(&clean, '`').saturating_add(1));
    let pad = clean.is_empty()
        || clean.starts_with('`')
        || clean.ends_with('`')
        || (clean.starts_with(' ') && clean.ends_with(' ') && clean.bytes().any(|b| b != b' '));
    if pad {
        format!("{ticks} {clean} {ticks}")
    } else {
        format!("{ticks}{clean}{ticks}")
    }
}

/// Incremental CommonMark fenced-code-block tracker operating in O(1) memory across arbitrary
/// chunk boundaries and multi-megabyte lines.
#[derive(Default)]
struct FenceTracker {
    open: Option<(char, usize)>,
    at_line_start: bool,
    indent: usize,
    fence_char: Option<char>,
    run: usize,
    in_run: bool,
    after_has_backtick: bool,
    after_only_ws: bool,
    invalid_line: bool,
    ended_with_nl: bool,
    /// The previous scalar was a `\r` that already ended its line; a following `\n` belongs to it.
    cr_carry: bool,
}

impl FenceTracker {
    fn new() -> Self {
        Self {
            open: None,
            at_line_start: true,
            indent: 0,
            fence_char: None,
            run: 0,
            in_run: false,
            after_has_backtick: false,
            after_only_ws: true,
            invalid_line: false,
            ended_with_nl: true,
            cr_carry: false,
        }
    }

    fn reset_line(&mut self) {
        self.at_line_start = true;
        self.indent = 0;
        self.fence_char = None;
        self.run = 0;
        self.in_run = false;
        self.after_has_backtick = false;
        self.after_only_ws = true;
        self.invalid_line = false;
    }

    fn finish_line(&mut self) {
        if !self.invalid_line && self.run >= 3 {
            if let Some(first) = self.fence_char {
                match self.open {
                    None => {
                        if !(first == '`' && self.after_has_backtick) {
                            self.open = Some((first, self.run));
                        }
                    }
                    Some((fch, flen)) if fch == first && self.run >= flen && self.after_only_ws => {
                        self.open = None;
                    }
                    Some(_) => {}
                }
            }
        }
        self.reset_line();
    }

    fn feed(&mut self, s: &str) {
        for ch in s.chars() {
            if ch == '\n' || ch == '\r' {
                let pair = ch == '\n' && self.cr_carry;
                self.cr_carry = ch == '\r';
                if !pair {
                    self.finish_line();
                }
                self.ended_with_nl = true;
                continue;
            }
            self.cr_carry = false;
            self.ended_with_nl = false;
            if self.invalid_line {
                continue;
            }
            if self.at_line_start {
                if ch == ' ' {
                    self.indent += 1;
                    if self.indent > 3 {
                        self.invalid_line = true;
                    }
                    continue;
                }
                self.at_line_start = false;
                if matches!(ch, '`' | '~') {
                    self.fence_char = Some(ch);
                    self.run = 1;
                    self.in_run = true;
                } else {
                    self.invalid_line = true;
                }
                continue;
            }
            if self.in_run {
                if Some(ch) == self.fence_char {
                    self.run += 1;
                    continue;
                }
                self.in_run = false;
                if self.run < 3 {
                    self.invalid_line = true;
                    continue;
                }
            }
            if ch == '`' {
                self.after_has_backtick = true;
            }
            if !matches!(ch, ' ' | '\t') {
                self.after_only_ws = false;
            }
        }
    }

    fn finish(mut self) -> (Option<(char, usize)>, bool) {
        if !self.ended_with_nl {
            self.finish_line();
        }
        (self.open, self.ended_with_nl)
    }
}

/// Track CommonMark fenced code blocks across `s` and return `(fence_char, fence_len)` if a block
/// remains unclosed at EOF (e.g. after hard byte truncation).
pub fn unclosed_fence(s: &str) -> Option<(char, usize)> {
    let mut tracker = FenceTracker::new();
    tracker.feed(s);
    tracker.finish().0
}

/// Append the closing fence when `body` leaves a code fence open, so the Markdown after it (the
/// next entry, the next section) is not read as code. Clean bodies borrow.
pub fn close_unclosed_fence(body: &str) -> Cow<'_, str> {
    match unclosed_fence(body) {
        None => Cow::Borrowed(body),
        Some((fch, flen)) => {
            let mut closed = String::with_capacity(body.len() + flen + 1);
            closed.push_str(body);
            if !body.ends_with(['\n', '\r']) {
                closed.push('\n');
            }
            closed.extend(std::iter::repeat_n(fch, flen));
            Cow::Owned(closed)
        }
    }
}

fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// True when `trimmed` matches a `cv distill` numbered section boundary (`## 0. Hand-off`,
/// `## 1. Grounding`, `## 2. Brief`, `## 2b. Earlier generations`, `## 3. Timeline`,
/// `## 4. Facts`, `## 5. Ledger`, `## 6. The last …`).
pub(crate) fn is_distill_section_heading(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("##") else {
        return false;
    };
    if rest.starts_with('#') || !rest.starts_with([' ', '\t']) {
        return false;
    }
    let after_hashes = rest.trim_start_matches([' ', '\t']);
    let digits = after_hashes.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=2).contains(&digits) {
        return false;
    }
    let after_digits = &after_hashes[digits..];
    let after_sub = after_digits
        .strip_prefix(['a', 'b', 'c', 'A', 'B', 'C'])
        .unwrap_or(after_digits);
    let Some(title) = after_sub.strip_prefix(". ") else {
        return false;
    };
    [
        "hand-off",
        "grounding",
        "brief",
        "earlier generations",
        "timeline",
        "facts",
        "ledger",
        "the last ",
        "prefix bricks",
        "continuation state",
        "verified progress",
        "reads behind later edits",
        "[unverified predecessor notes",
    ]
    .iter()
    .any(|sec| {
        starts_with_ignore_ascii_case(title, sec)
            && (sec.ends_with(' ')
                || title[sec.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| matches!(c, ' ' | '\t' | '(' | '[' | ':' | '—' | '-')))
    })
}

/// Maximum leading `' ' | '\t' | '>'` bytes inspected before a candidate header token.
const MAX_HEADER_PREFIX: usize = 64;

/// True when `trimmed` opens a CommonMark HTML block of kinds 1-5.
fn is_html_block_opener(trimmed: &str) -> bool {
    let Some(after_lt) = trimmed.strip_prefix('<') else {
        return false;
    };
    if after_lt.starts_with("!--") || after_lt.starts_with('?') || after_lt.starts_with("![CDATA[") {
        return true;
    }
    if after_lt
        .strip_prefix('!')
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_alphabetic()))
    {
        return true;
    }
    ["script", "pre", "style", "textarea"].iter().any(|tag| {
        starts_with_ignore_ascii_case(after_lt, tag)
            && after_lt[tag.len()..]
                .chars()
                .next()
                .is_none_or(|c| matches!(c, ' ' | '\t' | '>'))
    })
}

/// Rewrite a single line (without trailing `\n`/`\r`) when its leading non-whitespace/blockquote
/// tokens forge an authoritative role header or `cv` structural section marker.
fn rewrite_forged_line(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() && matches!(bytes[idx], b' ' | b'\t' | b'>') {
        idx += 1;
        if idx > MAX_HEADER_PREFIX {
            return None;
        }
    }
    let (prefix, trimmed) = line.split_at(idx);
    if trimmed.is_empty() {
        return None;
    }

    // 1. Bold speaker/turn labels (`**message to this agent**`, `**user:**`, `**↩ result`, ...).
    if let Some(after) = trimmed.strip_prefix("**") {
        let bold_role = [
            "message to this agent**",
            "message delivered while working**",
            "agent**",
            "agent:**",
            "agent said**",
            "user**",
            "user:**",
            "user (",
            "assistant**",
            "assistant:**",
            "assistant (",
            "system**",
            "system:**",
            "system (",
            "tool**",
            "tool:**",
            "tool (",
            "tool output**",
            "hook output**",
            "harness context**",
            "human**",
            "human:**",
            "↩ result",
            "🔧 ",
        ]
        .iter()
        .any(|pat| starts_with_ignore_ascii_case(after, pat));
        if bold_role {
            return Some(format!("{prefix}\\*\\*{after}"));
        }
    }

    // 2. Bracketed role headers (`[User]:`, `[System]:`, `[Message]`, `[cv distill]`, `[cv pack]`).
    if let Some(rest) = trimmed.strip_prefix('[') {
        let bracket_role = [
            "[user]:",
            "[system]:",
            "[assistant]:",
            "[human]:",
            "[tool]:",
            "[developer]:",
            "[model]:",
            "[message]",
            "[cv distill]",
            "[cv pack]",
        ]
        .iter()
        .any(|pat| starts_with_ignore_ascii_case(trimmed, pat));
        if bracket_role {
            return Some(match rest.find(']') {
                Some(close) => format!("{prefix}\\[{}\\]{}", &rest[..close], &rest[close + 1..]),
                None => format!("{prefix}\\[{rest}"),
            });
        }
    }

    // 3. ATX role headings (`# USER`, `## user`, `## system · ...`), `cv distill` section headings,
    // and numbered timeline headers (`### [42] ...`).
    if trimmed.starts_with('#') {
        if is_distill_section_heading(trimmed) {
            return Some(format!("{prefix}\\{trimmed}"));
        }
        let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
        if (1..=6).contains(&hashes) {
            let after_hashes = &trimmed[hashes..];
            if after_hashes.starts_with([' ', '\t']) {
                let after = after_hashes.trim_start_matches([' ', '\t']);
                let numbered_timeline = hashes == 3
                    && after.starts_with('[')
                    && after[1..].bytes().take_while(u8::is_ascii_digit).count() > 0
                    && after[1..]
                        .trim_start_matches(|c: char| c.is_ascii_digit())
                        .starts_with(']');
                let pack_title = starts_with_ignore_ascii_case(after, "distilled context —")
                    || starts_with_ignore_ascii_case(after, "context pack:")
                    || starts_with_ignore_ascii_case(after, "findings from another lane");
                let role_heading = ["user", "system", "assistant", "human", "tool", "developer"]
                    .iter()
                    .any(|role| {
                        starts_with_ignore_ascii_case(after, role)
                            && after[role.len()..]
                                .chars()
                                .next()
                                .is_none_or(|c| matches!(c, ' ' | '\t' | ':' | '(' | '·'))
                    });
                if numbered_timeline || pack_title || role_heading {
                    return Some(format!("{prefix}\\{trimmed}"));
                }
            }
        }
    }

    // 4. CommonMark HTML block openers.
    if is_html_block_opener(trimmed) {
        return Some(format!("{prefix}\\{trimmed}"));
    }

    // 5. System/prompt XML boundary tags (`</SYSTEM_MESSAGE>`, `<USER_REQUEST>`, `<system-reminder>`, ...).
    if let Some(after_lt) = trimmed.strip_prefix('<') {
        let tag = after_lt.strip_prefix('/').unwrap_or(after_lt);
        let system_tag = [
            "system_message",
            "system-reminder",
            "system_instruction",
            "system>",
            "system ",
            "user_request",
            "user>",
            "user ",
            "human>",
            "assistant>",
            "developer>",
            "task-notification",
            "local-command-stdout",
        ]
        .iter()
        .any(|pat| starts_with_ignore_ascii_case(tag, pat));
        if system_tag {
            return Some(format!("{prefix}<\\{after_lt}"));
        }
    }

    // 6. Bare role-colon & prompt-bundle speaker prefixes (`User: ...`, `System: ...`, `user said:`).
    for said in ["user said:", "assistant said:", "system said:", "tool said:"] {
        if starts_with_ignore_ascii_case(trimmed, said) {
            let colon = said.len() - 1;
            return Some(format!("{prefix}{}\\:{}", &trimmed[..colon], &trimmed[colon + 1..]));
        }
    }
    for role in ["user", "system", "human", "assistant", "developer"] {
        if starts_with_ignore_ascii_case(trimmed, role) {
            let rest = &trimmed[role.len()..];
            if let Some(after_colon) = rest.strip_prefix(':') {
                if after_colon.is_empty() || after_colon.starts_with([' ', '\t']) {
                    return Some(format!("{prefix}{}\\:{after_colon}", &trimmed[..role.len()]));
                }
            }
        }
    }

    // 7. Distill tail tool markers & workflow `--revive` cut boundaries.
    if [
        "▶ **",
        "◀ result",
        "◀ error",
        "--- 8< ---",
        "--- >8 ---",
        "--- RESUMING AN INTERRUPTED RUN ---",
        "--- END RESUME CONTEXT ---",
    ]
    .iter()
    .any(|pat| trimmed.starts_with(pat))
    {
        return Some(format!("{prefix}\\{trimmed}"));
    }

    None
}

/// Split `text` into `(line, ending)` pairs at every CommonMark line ending; `ending` is `"\n"`,
/// `"\r\n"`, a lone `"\r"`, or `""` for the final line.
pub(crate) fn lines_with_endings(text: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut rest = Some(text);
    std::iter::from_fn(move || {
        let r = rest?;
        let Some(p) = r.find(['\r', '\n']) else {
            rest = None;
            return Some((r, ""));
        };
        let len = if r[p..].starts_with("\r\n") { 2 } else { 1 };
        rest = Some(&r[p + len..]);
        Some((&r[..p], &r[p..p + len]))
    })
}

/// Neutralize line-leading forged role headers and structural markers inside untrusted
/// (`Origin != Origin::Human` or `ToolResult`) text rendered into flattened Markdown/prompt views.
/// Clean text returns [`Cow::Borrowed`] with zero allocation.
pub fn neutralize_forged_headers(text: &str) -> Cow<'_, str> {
    let dirty = lines_with_endings(text).any(|(line, ending)| ending == "\r" || rewrite_forged_line(line).is_some());
    if !dirty {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for (line, ending) in lines_with_endings(text) {
        match rewrite_forged_line(line) {
            Some(rewritten) => out.push_str(&rewritten),
            None => out.push_str(line),
        }
        out.push_str(if ending == "\r" { "\n" } else { ending });
    }
    Cow::Owned(out)
}

/// Preserve genuine `Origin::Human` text verbatim while neutralizing forged role/section headers
/// in any non-human turn (`Origin::Model`, `Origin::Subagent`, `Origin::Tool`, `Origin::System`,
/// `Origin::Import`, `Origin::Unknown`).
pub fn neutralize_non_human(origin: Origin, text: &str) -> Cow<'_, str> {
    if origin == Origin::Human {
        Cow::Borrowed(text)
    } else {
        neutralize_forged_headers(text)
    }
}

fn emit_tracked(bytes: &str, fences: &mut FenceTracker, out: &mut dyn Write) -> std::io::Result<()> {
    fences.feed(bytes);
    out.write_all(bytes.as_bytes())
}

fn flush_line_head(buf: &mut String, fences: &mut FenceTracker, out: &mut dyn Write) -> std::io::Result<()> {
    match rewrite_forged_line(buf) {
        Some(rewritten) => emit_tracked(&rewritten, fences, out)?,
        None => emit_tracked(buf, fences, out)?,
    }
    buf.clear();
    Ok(())
}

/// Streaming bounded-scratch writer for [`neutralize_forged_headers`]: buffers at most
/// `HEAD_CAP` (256) total bytes at the head of each line (including leading spaces/`>`) so
/// multi-megabyte lazy spans stream in O(1) RAM, and closes any unclosed Markdown code fence left
/// open at the end of a non-human text block so subsequent `## <turn>` headings cannot be swallowed.
pub(crate) fn write_neutralized_chunks(
    text: &crate::Text,
    resolver: &crate::Resolver,
    lead: &str,
    line_prefix: &str,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    const HEAD_CAP: usize = 256;
    let mut buf = String::with_capacity(HEAD_CAP + 4);
    let mut passthrough_line = false;
    let mut cr_carry = false;
    let mut fences = FenceTracker::new();
    emit_tracked(lead, &mut fences, out)?;
    text.try_for_each_chunk(resolver, 64 * 1024, |part| {
        let mut rest = part;
        if cr_carry && !rest.is_empty() {
            cr_carry = false;
            let ending = match rest.strip_prefix('\n') {
                Some(after) => {
                    rest = after;
                    "\r\n"
                }
                None => "\n",
            };
            emit_tracked(ending, &mut fences, out)?;
            emit_tracked(line_prefix, &mut fences, out)?;
        }
        while !rest.is_empty() {
            let eol = rest.find(['\r', '\n']);
            if passthrough_line {
                let Some(p) = eol else {
                    emit_tracked(rest, &mut fences, out)?;
                    break;
                };
                emit_tracked(&rest[..p], &mut fences, out)?;
                passthrough_line = false;
                rest = &rest[p..];
            } else {
                let need = HEAD_CAP.saturating_sub(buf.len());
                match eol {
                    Some(p) if p <= need => {
                        buf.push_str(&rest[..p]);
                        flush_line_head(&mut buf, &mut fences, out)?;
                        rest = &rest[p..];
                    }
                    _ => {
                        let mut take = rest.floor_char_boundary(eol.unwrap_or(rest.len()).min(need));
                        if take == 0 {
                            take = rest.chars().next().map_or(0, char::len_utf8);
                        }
                        buf.push_str(&rest[..take]);
                        rest = &rest[take..];
                        if buf.len() >= HEAD_CAP {
                            flush_line_head(&mut buf, &mut fences, out)?;
                            passthrough_line = true;
                        }
                        continue;
                    }
                }
            }
            let ending = if rest.starts_with("\r\n") {
                "\r\n"
            } else if rest == "\r" {
                cr_carry = true;
                break;
            } else {
                "\n"
            };
            rest = &rest[if ending == "\r\n" { 2 } else { 1 }..];
            emit_tracked(ending, &mut fences, out)?;
            emit_tracked(line_prefix, &mut fences, out)?;
        }
        Ok(())
    })?;
    if cr_carry {
        emit_tracked("\n", &mut fences, out)?;
        emit_tracked(line_prefix, &mut fences, out)?;
    }
    if !buf.is_empty() {
        flush_line_head(&mut buf, &mut fences, out)?;
    }
    let (unclosed, ended_with_nl) = fences.finish();
    if let Some((fch, flen)) = unclosed {
        if !ended_with_nl {
            out.write_all(b"\n")?;
        }
        let closing: String = std::iter::repeat_n(fch, flen).collect();
        out.write_all(closing.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::sanitize_line;
    use std::borrow::Cow;

    #[test]
    fn intermediate_esc_and_c1_sequences_do_not_leak_payload_text() {
        assert_eq!(sanitize_line("a\u{1b}(Bé\u{1b}#8z"), "aéz");
        assert_eq!(sanitize_line("a\u{9b}31mred\u{9b}0mz"), "aredz");
        for start in ['\u{90}', '\u{98}', '\u{9d}', '\u{9e}', '\u{9f}'] {
            assert_eq!(sanitize_line(&format!("a{start}hidden\u{9c}z")), "az");
            assert_eq!(sanitize_line(&format!("a{start}hidden\u{1b}\\z")), "az");
            assert_eq!(sanitize_line(&format!("a{start}unterminated")), "a");
        }
        assert_eq!(sanitize_line("a\u{1b}(é"), "aé");
    }

    #[test]
    fn clean_text_passes_through_borrowed() {
        let s = "fix the zebrafish migration ✦ (rev 2)";
        assert!(matches!(sanitize_line(s), Cow::Borrowed(b) if b == s));
    }

    #[test]
    fn strips_osc_title_bomb_and_csi_colors() {
        // OSC terminated by BEL, then a raw CSI color sequence.
        assert_eq!(sanitize_line("evil\u{1b}]0;pwned\u{7}task"), "eviltask");
        assert_eq!(sanitize_line("\u{1b}[31mred\u{1b}[0m"), "red");
        // OSC terminated by ST (ESC \).
        assert_eq!(sanitize_line("\u{1b}]0;x\u{1b}\\done"), "done");
        // Multi-parameter CSI.
        assert_eq!(sanitize_line("a\u{1b}[1;31;4mb"), "ab");
    }

    #[test]
    fn structural_whitespace_becomes_single_spaces() {
        assert_eq!(sanitize_line("a\nb\tc\r\nd"), "a b c  d");
    }

    #[test]
    fn drops_bare_controls_del_and_lone_esc() {
        assert_eq!(sanitize_line("a\u{8}b\u{7f}c\u{1b}"), "abc");
        // Unterminated OSC eats to end of string rather than leaking payload.
        assert_eq!(sanitize_line("x\u{1b}]0;never terminated"), "x");
        // Two-char ESC sequence: both chars dropped.
        assert_eq!(sanitize_line("a\u{1b}cb"), "ab");
    }

    #[test]
    fn strips_trojan_source_bidi_format_codepoints_in_both_sanitizers() {
        let bidi_all = [
            '\u{202A}', // LRE
            '\u{202B}', // RLE
            '\u{202C}', // PDF
            '\u{202D}', // LRO
            '\u{202E}', // RLO
            '\u{2066}', // LRI
            '\u{2067}', // RLI
            '\u{2068}', // FSI
            '\u{2069}', // PDI
        ];
        // Pure-bidi payload without C0/C1 controls must NOT bypass the Cow::Borrowed fast path.
        for cp in bidi_all {
            let poisoned = format!("safe{cp}spoof");
            let cleaned = sanitize_line(&poisoned);
            assert!(matches!(cleaned, Cow::Owned(_)), "bidi {cp:?} bypassed borrowed check");
            assert_eq!(cleaned, "safespoof");

            let mut buf = String::from("prefix:");
            super::strip_ansi_into(&poisoned, &mut buf);
            assert_eq!(buf, "prefix:safespoof");
        }

        // Full Trojan-Source RLO/PDI spoofing vector mixed with ANSI CSI + multiline structural text.
        let trojan = "if access_level != \"user\u{202E} \u{2066}// Check admin\u{2069} \u{202C}\"\n\t\u{1b}[31mreturn\u{1b}[0m;\r\n";
        let mut out = String::new();
        super::strip_ansi_into(trojan, &mut out);
        assert_eq!(out, "if access_level != \"user // Check admin \"\n\treturn;\n");
        assert!(!out.chars().any(super::is_bidi_control));

        let line = sanitize_line(trojan);
        assert_eq!(line, "if access_level != \"user // Check admin \"  return;  ");
        assert!(!line.chars().any(super::is_bidi_control));
    }
}
