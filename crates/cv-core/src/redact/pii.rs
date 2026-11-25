//! Optional pii@1 projection for text and machine identity.
use regex::{Captures, Regex};
use std::sync::LazyLock;

pub fn reduce_text(text: &str, home: Option<&str>) -> String {
    static EMAIL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b").unwrap());
    static PHONE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:\+\d{1,3}[ .-]?)?(?:\(\d{2,4}\)[ .-]?)?\d{2,4}(?:[ .-]?\d{2,4}){1,3}").unwrap()
    });
    static DATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:19|20)\d{2}-\d\d-\d\d(?: \d\d)?$").unwrap());
    let text = EMAIL.replace_all(text, "[email]");
    let text = PHONE.replace_all(&text, |c: &Captures<'_>| {
        let m = c.get(0).unwrap();
        let s = m.as_str();
        let digits = s.bytes().filter(u8::is_ascii_digit).count();
        let boundary = |c: char| c.is_alphanumeric() || c == '_';
        if (7..=15).contains(&digits)
            && s.contains(['+', '(', ')', ' ', '-'])
            && !DATE.is_match(s)
            && !text[..m.start()]
                .chars()
                .next_back()
                .is_some_and(|c| boundary(c) || c == '.')
            && !text[m.end()..].chars().next().is_some_and(boundary)
        {
            "[phone]".into()
        } else {
            s.to_owned()
        }
    });
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return text.into_owned();
    };
    let mut out = String::new();
    let mut last = 0;
    for (i, _) in text.match_indices(home) {
        let end = i + home.len();
        if !text[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            && text[end..]
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || matches!(c, '/' | '"' | '\'' | '`'))
        {
            out.push_str(&text[last..i]);
            out.push('~');
            last = end;
        }
    }
    out.push_str(&text[last..]);
    out
}

pub fn machine(name: &str) -> String {
    crate::digest::sha256_hex(name.as_bytes())[..8].into()
}

#[cfg(test)]
mod tests {
    #[test]
    fn redact_pinned_pii_vectors() {
        let suite: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/redaction/pii.json")).unwrap();
        for c in suite["cases"].as_array().unwrap() {
            assert_eq!(
                super::reduce_text(c["in"].as_str().unwrap(), c["home"].as_str()),
                c["out"].as_str().unwrap(),
                "{}",
                c["name"]
            );
        }
        for c in suite["session"].as_array().unwrap() {
            assert_eq!(
                serde_json::json!({"machine":super::machine(c["in"]["machine"].as_str().unwrap())}),
                c["out"]
            );
        }
    }
}
