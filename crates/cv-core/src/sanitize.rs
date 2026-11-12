//! One shared sanitizer for terminal-bound untrusted text (task titles/bodies/notes, endpoints).
//!
//! The task log is replayed forever, so a poisoned record can never be cured at the
//! send path — every *reader* strips control characters itself (comms canon: "every consumer
//! sanitizes untrusted message text itself"). One ESC sequence in a task title would otherwise
//! corrupt every terminal that ever runs `cv task list`, eternally.
//!
//! Scope decision: this is applied at **terminal render sites only** (CLI print paths, cockpit
//! rows). cv serves JSON, where escaping is the transport's job — serde_json escapes control
//! characters into harmless `\uXXXX` — so JSON responses stay raw and lossless for foreign
//! clients to sanitize at *their* render sites.

use std::borrow::Cow;

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
