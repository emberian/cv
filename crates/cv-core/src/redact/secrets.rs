// SPDX-License-Identifier: Apache-2.0
//! Structure-aware credential recognizers, adapted from SessionPipe's secrets@1.
//! See tests/fixtures/redaction for the pinned vectors and upstream notices.
use regex::Regex;
use serde_json::Value;
use std::{borrow::Cow, sync::LazyLock};

const MARK: &str = "[redacted]";
macro_rules! re {
    ($pattern:expr) => {{
        static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new($pattern).expect("credential pattern"));
        &*RE
    }};
}

fn standin(v: &str) -> bool {
    v.is_empty() || v.starts_with("[REDACTED:") && v.ends_with(']') || re!(r"(?ix)^(?:string|number|boolean|bool|str|int|any|unknown|null|undefined|none|nil|true|false|optional|required|text|varchar|password|secret|token|env|string\[\]|object|bytes|\[?redacted\]?|\*+|x{3,}|\.{3}|…|<[a-z_][a-zA-Z0-9_.-]{0,79}>|\$\{[a-z_][a-zA-Z0-9_.]{0,79}\}|\{\{\x20?[a-z_][a-zA-Z0-9_.-]{0,79}\x20?\}\}|\$[a-z_][a-zA-Z0-9_]{0,79}|%[a-z_][a-zA-Z0-9_]{0,79}%)$").is_match(v)
}

fn placeholder(v: &str) -> bool {
    let s = v.trim_end_matches(['.', ',', ';', ':', '!', '?']);
    standin(s) || s.starts_with(['<','{','$','%']) || s.contains('(') ||
        re!(r"(?i)^(?:(?:process\.env|env|os\.environ|c\.env|this\.env|import\.meta\.env|getenv|config|settings|secrets|args|opts|options|req|request|body|params|input|data|user|self|this)\b|(?:your|my|the|a|an|some|new|old)[-_ ]|[a-zA-Z0-9_$]+(?:\.[a-zA-Z0-9_$]+)+$)").is_match(s)
}

fn keep(name: &str, v: &str) -> bool {
    placeholder(v)
        || v.contains(MARK)
        || v.contains("[REDACTED:")
        || (name
            .trim_end_matches(['S', 's'])
            .to_ascii_lowercase()
            .ends_with("token")
            && re!(r"(?i)^\d+(?:[.,]\d+)*[km]?$").is_match(v))
}

fn secret_name(name: &str) -> bool {
    if name.ends_with("_KEY")
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return true;
    }
    let flat = name.to_ascii_lowercase().replace(['_', '-'], "");
    re!(r"(?:pass(?:word|wd|phrase|code)|pwd|secret|secretkey|secretaccesskey|clientsecret|apikey|accesskey|accesstoken|authtoken|refreshtoken|privatekey|token|credential|creds)s?$").is_match(&flat)
}

fn cookie(name: &str) -> bool {
    name.trim_end_matches('s').ends_with("cookie")
}
fn header(name: &str) -> bool {
    cookie(name) || name.ends_with("authorization")
}

pub(super) fn context_key(name: &str) -> bool {
    let low = name.to_ascii_lowercase();
    matches!(low.as_str(), "name" | "key" | "value") || header(&low) || secret_name(name)
}

fn whole(name: &str, v: &str, text: &str, wire: bool) -> bool {
    let low = name.to_ascii_lowercase();
    if name == "{jar}" {
        text.len() > 5
            && !text.chars().any(char::is_whitespace)
            && !standin(text)
            && !re!(r"(?s)^(?:[\p{L}\p{M}'’.-]+|https?://.*)$").is_match(text)
    } else if header(&low) {
        (!standin(v.trim()) && !v.contains("[REDACTED:") && (wire || !v.contains(MARK))) || text != v
    } else {
        secret_name(name) && !keep(name, v)
    }
}

const ATTRS: [&str; 6] = ["domain", "path", "expires", "httponly", "secure", "samesite"];

struct Names {
    jar: bool,
    named: Option<String>,
}
impl Names {
    fn new<'a>(parent: &str, entries: impl Iterator<Item = (&'a str, Option<&'a str>)>) -> Self {
        let (mut name, mut key, mut has_name, mut has_value, mut marks) = (None, None, false, false, 0);
        for (k, v) in entries {
            match k.to_ascii_lowercase().as_str() {
                "name" => {
                    name = v;
                    has_name = true
                }
                "key" => key = v,
                "value" => has_value = true,
                k if ATTRS.contains(&k) => marks += 1,
                _ => {}
            }
        }
        let jar_parent = cookie(&parent.to_ascii_lowercase());
        let named = if jar_parent || (marks > 1 && name.or(key).is_some()) {
            Some("cookie")
        } else {
            name.or(key)
                .filter(|s| re!(r"^[a-zA-Z_][a-zA-Z0-9_.-]{0,79}$").is_match(s))
        };
        Self {
            jar: jar_parent && !has_name && !has_value && marks == 0,
            named: named.map(str::to_owned),
        }
    }
    fn child<'a>(&'a self, key: &'a str) -> &'a str {
        if self.jar {
            "{jar}"
        } else if key.eq_ignore_ascii_case("value") {
            self.named.as_deref().unwrap_or(key)
        } else {
            key
        }
    }
}

/// A whole value that an earlier pass already replaced with a `[REDACTED:<label>]` marker.
fn redacted_marker(s: &str) -> bool {
    s.strip_prefix("[REDACTED:")
        .and_then(|rest| rest.strip_suffix(']'))
        .is_some_and(|label| !label.is_empty() && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
}

pub(crate) fn structured(value: &mut Value, parent: &str) -> usize {
    walk(value, parent, true)
}
pub(crate) fn fields(value: &mut Value, parent: &str) -> usize {
    walk(value, parent, false)
}

fn walk(value: &mut Value, parent: &str, wire: bool) -> usize {
    match value {
        Value::String(s) => {
            if !wire && parent != "{jar}" && !context_key(parent) {
                return 0;
            }
            if wire && super::escaped_envelope(s) {
                if let Ok(mut inner) = serde_json::from_str::<Value>(s) {
                    let changes = structured(&mut inner, "");
                    if changes != 0 {
                        let encoded = inner.to_string();
                        *s = if whole(parent, s, &encoded, true) {
                            MARK.to_owned()
                        } else {
                            encoded
                        };
                        return changes;
                    }
                }
            }
            let next = text(s);
            if whole(parent, s, &next, wire) {
                if s == MARK || redacted_marker(s) {
                    return 0;
                }
                *s = if wire { MARK } else { "[REDACTED:secret]" }.to_string();
                1
            } else if wire {
                if let Cow::Owned(next) = next {
                    *s = next;
                    return 1;
                }
                0
            } else {
                0
            }
        }
        Value::Object(map) => {
            let names = Names::new(parent, map.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            map.iter_mut().map(|(k, v)| walk(v, names.child(k), wire)).sum()
        }
        Value::Array(items) => {
            let key = if header(&parent.to_ascii_lowercase()) {
                parent
            } else {
                ""
            };
            items.iter_mut().map(|v| walk(v, key, wire)).sum()
        }
        _ => 0,
    }
}

fn looks_generated(value: &str) -> bool {
    let s = value.trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
    if s.len() < 6
        || placeholder(s)
        || re!(r"(?i)^https?://").is_match(s)
        || re!(r"^(?:[a-z]+(?:[-'][a-z]+)*|[A-Z][a-z]+|\d+(?:[.,]\d+)*[a-zA-Z%]*|[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+)$")
            .is_match(s)
    {
        return false;
    }
    let digit = s.bytes().any(|b| b.is_ascii_digit());
    let lower = s.bytes().any(|b| b.is_ascii_lowercase());
    let upper = s.bytes().any(|b| b.is_ascii_uppercase());
    let other = s.bytes().any(|b| !b.is_ascii_alphanumeric());
    [digit, lower, upper, other].into_iter().filter(|b| *b).count() >= 3 || digit && (lower || upper)
}

type Cuts = Vec<(usize, usize)>;
fn cut<'a>(s: &'a str, mut cuts: Cuts) -> Cow<'a, str> {
    if cuts.is_empty() {
        return Cow::Borrowed(s);
    }
    cuts.sort_unstable_by_key(|&(a, b)| (a, std::cmp::Reverse(b)));
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (a, b) in cuts {
        if a < last {
            last = last.max(b);
            continue;
        }
        out.push_str(&s[last..a]);
        out.push_str(MARK);
        last = b;
    }
    out.push_str(&s[last..]);
    Cow::Owned(out)
}

fn assignments(s: &str, cuts: &mut Cuts) {
    let sep = re!(r#"(?i)\b([a-z][a-z0-9_-]{1,79})(?:["']?\s{0,3}(?::=|=>|=|:)\s{0,3})"#);
    let mut start = 0;
    while let Some(c) = sep.captures_at(s, start) {
        let m = c.get(0).unwrap();
        start = m.end();
        if !secret_name(&c[1]) {
            continue;
        }
        let rest = &s[start..];
        let Some(val) = re!(r#"^(?:"([^"\n]{1,300})"|'([^'\n]{1,300})'|`([^`\n]{1,300})`|([^\s"'`,;&)}\]]{1,300}))"#)
            .captures(rest)
        else {
            continue;
        };
        let v = val.iter().skip(1).flatten().next().unwrap();
        let prose = val.get(4).is_some()
            && m.as_str().trim_end().ends_with(':')
            && matches!(v.as_str(), "use" | "set")
            && rest[v.end()..].starts_with(' ');
        if !prose && !keep(&c[1], v.as_str()) {
            cuts.push((start + v.start(), start + v.end()));
        }
        start += val.get(0).unwrap().end();
    }
    let prose = re!(
        r#"(?i)\b(?:pass(?:word|wd|phrase|code)?s?|pwd|secret(?:[ _-]?key)?s?|client[ _-]?secret|api[ _-]?keys?|access[ _-]?keys?|(?:access|auth|refresh)[ _-]?tokens?|private[ _-]?keys?|tokens?|credentials?|creds)(?:[ \t]{1,3}(?:is|was|=|:|to|as|of|for[ \t]{1,3}\S{1,40}[ \t]{1,3}is))?[ \t]{1,3}["'`]?([^\s"'`]{6,200})"#
    );
    for c in prose.captures_iter(s) {
        let v = c.get(1).unwrap();
        if looks_generated(v.as_str()) && !v.as_str().contains(MARK) {
            cuts.push((v.start(), v.end()));
        }
    }
}

fn query(s: &str, cuts: &mut Cuts) {
    for c in re!(r"(?i)(?:&amp;|[?&])(k|key|sig|signature|x-amz-signature|x-goog-signature)=([A-Za-z0-9._~%+/=-]{16,})")
        .captures_iter(s)
    {
        let v = c.get(2).unwrap();
        let words = matches!(c[1].to_ascii_lowercase().as_str(), "k" | "key")
            && re!(r"%20|[+_.-]")
                .split(v.as_str().trim_end_matches('.'))
                .all(|w| re!(r"^(?:[a-z]+|[A-Z][a-z]+|[0-9]{1,4})$").is_match(w));
        if !words {
            cuts.push((v.start(), v.end()));
        }
    }
}

fn quote(s: &[u8], at: usize) -> Option<(usize, u8)> {
    let n = s.get(at..)?.iter().take_while(|&&b| b == b'\\').count();
    let q = *s.get(at + n)?;
    (matches!(n, 0 | 1 | 3 | 7) && matches!(q, b'\'' | b'"' | b'`')).then_some((n, q))
}
fn end_string(s: &[u8], mut i: usize, n: usize, q: u8, multiline: bool) -> Option<usize> {
    while i < s.len() {
        let slashes = s[i..].iter().take_while(|&&b| b == b'\\').count();
        i += slashes;
        match s.get(i) {
            Some(&b) if b == q && slashes % (2 * (n + 1)) == n => return Some(i - n),
            Some(b'\n') if !multiline => return None,
            _ => i += 1,
        }
    }
    Some(s.len())
}
fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$')
}
fn run(b: &[u8], mut i: usize, pred: impl Fn(u8) -> bool) -> usize {
    while b.get(i).is_some_and(|&c| pred(c)) {
        i += 1
    }
    i
}
fn cookie_value(b: &[u8], start: usize, date: bool, within: &[u8]) -> usize {
    if let Some((n, q)) = quote(b, start) {
        if &b[start..=start + n] != within {
            let mut i = start + n + 1;
            while i < b.len() {
                let j = run(b, i, |c| c == b'\\');
                match b.get(j) {
                    Some(c) if *c == q && j - i < n => return i,
                    Some(c) if *c == q && (j - i) % (2 * (n + 1)) == n => return j + 1,
                    Some(b'\n' | b'\r') => return j,
                    _ => i = j + 1,
                }
            }
            return b.len();
        }
    }
    if matches!(b.get(start), Some(b'{' | b'[')) {
        let (mut i, mut depth) = (start, 0usize);
        while i < b.len() {
            if let Some((n, q)) = quote(b, i) {
                if &b[i..=i + n] == within {
                    return i;
                }
                match end_string(b, i + n + 1, n, q, false) {
                    Some(end) => i = (end + n + 1).min(b.len()),
                    None => return run(b, i, |c| !matches!(c, b'\n' | b'\r')),
                }
            } else {
                match b[i] {
                    b'\r' | b'\n' => return i,
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return i + 1;
                        }
                    }
                    _ => {}
                }
                i = if b[i] == b'\\' {
                    run(b, i, |c| c == b'\\')
                } else {
                    i + 1
                };
            }
        }
        return i;
    }
    let ok = |c: u8| {
        !matches!(c, b'\'' | b'"' | b'`' | b';' | b'\\' | b'\r' | b'\n')
            && (date || (!c.is_ascii_whitespace() && c != b','))
    };
    let mut i = start;
    loop {
        i = run(b, i, ok);
        if !matches!(b.get(i), Some(b',' | b'\\'))
            || !b.get(i + 1).is_some_and(|&x| ok(x))
            || (b[i] == b'\\' && matches!(b[i + 1], b'r' | b'n' | b't'))
        {
            return i;
        }
        i += 1;
    }
}

fn cookies(s: &str, cuts: &mut Cuts) {
    let re = re!(
        r#"(?i)cookies?(?:(?:\\{0,3}["'`])?\]?\s{0,3}[:=]\s{0,3}\$?(?:\\{0,3}["'`])?|\\{0,3}["'`]\s{0,3},\s{0,3}(?:\\{0,3}["'`])?|\s{1,3}\$?\\{0,3}["'`])|(?:added|replaced) cookie\s{1,3}|--cookie\s{1,3}\$?(?:\\{0,3}["'`])?|-b\s{1,3}\$?\\{0,3}["'`]"#
    );
    let b = s.as_bytes();
    let quote_before = |end: usize| {
        let mut i = end;
        if i > 0 && matches!(b[i - 1], b'"' | b'\'' | b'`') {
            i -= 1;
            while i > 0 && b[i - 1] == b'\\' && end - i < 8 {
                i -= 1;
            }
        }
        &b[i..end]
    };
    let mut pos = 0;
    while let Some(m) = re.find_at(s, pos) {
        let from = m.end();
        pos = from;
        if m.as_str().starts_with('-') && m.start() > 0 && (word(b[m.start() - 1]) || b[m.start() - 1] == b'-') {
            continue;
        }
        let mut within = quote_before(from);
        if within.is_empty() {
            let mut name = m.start();
            while name > 0 && (word(b[name - 1]) || b[name - 1] == b'-') {
                name -= 1
            }
            within = quote_before(name);
        }
        let pair = |i: usize, bare: bool| {
            let eq = run(b, i, |c| word(c) || c >= 128 || b"!#*+.^|~-".contains(&c));
            if eq == i {
                return None;
            }
            if b.get(eq) != Some(&b'=') {
                return bare.then_some(eq);
            }
            Some(cookie_value(
                b,
                eq + 1,
                s[i..eq].eq_ignore_ascii_case("expires"),
                within,
            ))
        };
        let Some(mut end) = pair(from, false) else { continue };
        while matches!(b.get(end), Some(b';' | b',')) {
            let next = run(b, end + 1, |c| matches!(c, b' ' | b'\t'));
            let Some(more) = pair(next, b[end] == b';') else { break };
            end = more;
        }
        let value = &s[from..end];
        if !matches!(value.to_ascii_lowercase().as_str(), "name=value" | "key=value")
            && value.split_once('=').is_some_and(|(_, v)| !standin(v))
        {
            cuts.push((from, end));
            pos = end;
        }
    }
}

struct Frame<'a> {
    object: bool,
    cookie: bool,
    fields: Vec<(&'a str, Option<&'a str>)>,
    spans: Vec<(&'a str, usize, usize, usize, usize)>,
}
fn close(frame: Frame<'_>, s: &str, offset: usize, cuts: &mut Cuts) {
    let names = Names::new(if frame.cookie { "cookie" } else { "" }, frame.fields.into_iter());
    for (key, start, end, first, last) in frame.spans {
        let name = names.child(key);
        if name == key {
            continue;
        }
        let v = &s[start..end];
        let inner = cuts[first..last]
            .iter()
            .map(|&(a, b)| (a - start - offset, b - start - offset))
            .collect();
        if whole(name, v, &cut(v, inner), true) {
            cuts.push((offset + start, offset + end));
        }
    }
}

fn named(s: &str, offset: usize, depth: usize, cuts: &mut Cuts) {
    let b = s.as_bytes();
    let mut stack: Vec<Frame<'_>> = Vec::new();
    let (mut key, mut prev, mut last_word, mut word_end) = (None::<&str>, 0u8, "", usize::MAX);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if word(c) {
            let end = run(b, i, word);
            let after = run(b, end, |x| x.is_ascii_whitespace());
            last_word = &s[i..end];
            word_end = end;
            i = end;
            if re!(r"(?i)^[rbfu]{1,2}$").is_match(last_word) && quote(b, end).is_some() {
                continue;
            }
            if b.get(after) == Some(&b':') && stack.last().is_some_and(|f| f.object) && matches!(prev, b'{' | b',') {
                key = Some(last_word);
                i = after + 1;
                prev = b':';
                continue;
            }
            if let (Some(f), Some(k)) = (stack.last_mut(), key.take()) {
                f.fields.push((k, None));
            }
            prev = b'w';
            continue;
        }
        let marks=word_end==i && ((c==b'"' && last_word.as_bytes().last().is_some_and(u8::is_ascii_digit))
            || (c==b'\'' && !re!(r"(?i)^(?:[rbfu]{1,2}|return|case|typeof|throw|yield|await|else|in|of|new|delete|void|instanceof)$").is_match(last_word)));
        if let Some((n, q)) = quote(b, i).filter(|_| !marks) {
            let from = i + n + 1;
            if let Some(to) = end_string(b, from, n, q, q == b'`') {
                let end = (to + n + 1).min(b.len());
                if !re!(r#"^[a-zA-Z0-9_$-]{1,80}["'`]\s{0,3}:"#).is_match(&s[end..]) {
                    let after = run(b, end, |x| x.is_ascii_whitespace());
                    let value = &s[from..to];
                    if b.get(after) == Some(&b':') {
                        key = Some(value);
                        i = after + 1;
                        prev = b':';
                        continue;
                    }
                    let first = cuts.len();
                    if depth < 3 && value.contains(['{', '[', '\\']) {
                        named(value, offset + from, depth + 1, cuts);
                    }
                    if let (Some(f), Some(k)) = (stack.last_mut().filter(|f| f.object), key.take()) {
                        f.fields.push((k, Some(value)));
                        f.spans.push((k, from, to, first, cuts.len()));
                    }
                    i = end;
                    prev = b'"';
                    continue;
                }
            }
        }
        match c {
            b'\\' => {
                i = run(b, i, |v| v == b'\\');
                continue;
            }
            b'{' | b'[' => {
                if let (Some(f), Some(k)) = (stack.last_mut(), key) {
                    f.fields.push((k, None));
                }
                let is_cookie = key.map_or_else(
                    || stack.last().is_some_and(|f| !f.object && f.cookie),
                    |k| cookie(&k.to_ascii_lowercase()),
                );
                stack.push(Frame {
                    object: c == b'{',
                    cookie: is_cookie,
                    fields: vec![],
                    spans: vec![],
                });
                key = None;
            }
            b'}' | b']' => {
                if let Some(f) = stack.pop() {
                    close(f, s, offset, cuts)
                }
                key = None;
            }
            _ => key = (matches!(c, b'(' | b'=') && prev == b'w').then_some(last_word),
        }
        prev = c;
        i += 1;
    }
    while let Some(f) = stack.pop() {
        close(f, s, offset, cuts);
    }
}

fn replace<'a>(input: Cow<'a, str>, f: impl FnOnce(&str, &mut Cuts)) -> Cow<'a, str> {
    let mut cuts = vec![];
    f(&input, &mut cuts);
    if cuts.is_empty() {
        input
    } else {
        Cow::Owned(cut(&input, cuts).into_owned())
    }
}

pub(super) fn text(s: &str) -> Cow<'_, str> {
    let out = replace(Cow::Borrowed(s), |s, cuts| {
        for m in vendor().find_iter(s) {
            cuts.push((m.start(), m.end()));
        }
        for re in [
            re!(r#"(?i)\b[a-z][a-z0-9+.-]{1,20}://[^\s:/@'"`<>]{1,100}:([^\s@/'"`<>]{1,200})@"#),
            re!(r"(?i)\b(?:Bearer|Basic|Token)\s+([A-Za-z0-9._~+/=-]{16,})"),
        ] {
            for c in re.captures_iter(s) {
                let v = c.get(1).unwrap();
                cuts.push((v.start(), v.end()));
            }
        }
    });
    contexts(out)
}

fn contexts(out: Cow<'_, str>) -> Cow<'_, str> {
    let out = replace(out, assignments);
    let out = replace(out, cookies);
    let out = replace(out, |s, cuts| {
        if re!(r"(?i)value|cookie").is_match(s) {
            named(s, 0, 0, cuts)
        }
    });
    replace(out, query)
}

/// Extend existing credential classes without changing their class-specific markers.
pub(super) fn additional(s: &str, classes: &super::RedactClasses) -> Option<(String, usize)> {
    let mut out = Cow::Borrowed(s);
    if classes.api_keys {
        out = replace(out, |s, cuts| {
            for m in vendor().find_iter(s) {
                if !m.as_str().starts_with("-----BEGIN") && !m.as_str().starts_with("eyJ") {
                    cuts.push((m.start(), m.end()));
                }
            }
        });
    }
    if classes.assignments {
        if let Some(encoded) = embedded(&out, 0) {
            out = Cow::Owned(encoded);
        }
        out = contexts(out);
        out = replace(out, |s, cuts| {
            for c in re!(r#"(?i)(?:&amp;|[?&])(?:key|token|sig|x-amz-signature)=([^&\s#"'<>]+)"#).captures_iter(s) {
                let v = c.get(1).unwrap();
                if !v.as_str().contains(MARK) && !v.as_str().contains("[REDACTED:") && !standin(v.as_str()) {
                    cuts.push((v.start(), v.end()));
                }
            }
        });
    }
    match out {
        Cow::Borrowed(_) => None,
        Cow::Owned(value) => {
            let count = value.matches(MARK).count().max(1);
            Some((value.replace(MARK, "[REDACTED:secret]"), count))
        }
    }
}

// Decode a JSON string envelope before examining its credential keys. Preserve clean inputs byte for byte.
fn has_embedded_candidate(s: &str) -> bool {
    re!(r"(?i)key|pass|pwd|secret|token|cred|cookie|authorization|httponly|samesite|expires|domain|\\u").is_match(s)
}

fn embedded(s: &str, depth: usize) -> Option<String> {
    if !s.trim_start().starts_with(['{', '[', '"']) || !s.contains(':') || !has_embedded_candidate(s) {
        return None;
    }
    let mut value: Value = serde_json::from_str(s).ok()?;
    let changed = match &mut value {
        Value::Object(_) | Value::Array(_) => fields(&mut value, "") > 0,
        Value::String(inner) if depth < 8 => {
            *inner = embedded(inner, depth + 1)?;
            true
        }
        Value::String(_) => return Some("[REDACTED:secret]".into()),
        _ => false,
    };
    changed.then(|| value.to_string())
}

fn vendor() -> &'static Regex {
    re!(concat!(
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----[\s\S]*?(?:-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----|$)",
        r"|\b(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}\b",
        r"|\bgh[pousr]_[A-Za-z0-9]{30,}\b",
        r"|\bgithub_pat_[A-Za-z0-9_]{30,}\b",
        r"|\bglpat-[A-Za-z0-9_-]{20,}\b",
        r"|\bxox[abposre]-[A-Za-z0-9-]{10,}\b",
        r"|\bhttps://hooks\.slack\.com/services/[A-Za-z0-9/_-]+",
        r"|\bhttps://(?:ptb\.|canary\.)?discord(?:app)?\.com/api/webhooks/[0-9]+/[A-Za-z0-9_-]+",
        r"|\bsk-(?:ant-|proj-|svcacct-|admin-|or-v1-)?[A-Za-z0-9_-]{20,}\b",
        r"|\b(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,}\b",
        r"|\bwhsec_[A-Za-z0-9+/=]{20,}",
        r"|\bAIza[0-9A-Za-z_-]{35}\b",
        r"|\bya29\.[0-9A-Za-z_-]{20,}",
        r"|\b1//0[0-9A-Za-z_-]{30,}",
        r"|\bGOCSPX-[0-9A-Za-z_-]{20,}",
        r"|\bxai-[A-Za-z0-9]{30,}\b",
        r"|\bgsk_[A-Za-z0-9]{30,}\b",
        r"|\bpplx-[A-Za-z0-9]{30,}\b",
        r"|\bhf_[A-Za-z0-9]{30,}\b",
        r"|\bpcsk_[A-Za-z0-9_]{30,}\b",
        r"|\bnpm_[A-Za-z0-9]{30,}\b",
        r"|\bpypi-[A-Za-z0-9_-]{50,}",
        r"|\bSG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}",
        r"|\bshp(?:at|ss|ca|pa)_[a-fA-F0-9]{32}\b",
        r"|\bdop_v1_[a-f0-9]{64}\b",
        r"|\b(?:lin_api|lin_oauth)_[A-Za-z0-9]{30,}\b",
        r"|\b(?:ntn|secret)_[A-Za-z0-9]{40,}\b",
        r"|\bsbp_[a-f0-9]{40}\b",
        r"|\bfigd_[A-Za-z0-9_-]{30,}",
        r"|\bdp\.(?:pt|st|sa|ct)\.[A-Za-z0-9]{30,}",
        r"|\bgrn_[A-Za-z0-9_-]{20,}",
        r"|\bss_[a-f0-9]{32,}\b",
        r"|\b[0-9]{8,10}:AA[A-Za-z0-9_-]{33}\b",
        r"|\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redact_pinned_secrets_vectors() {
        let suite: Value = serde_json::from_str(include_str!("../../tests/fixtures/redaction/secrets.json")).unwrap();
        let mut failures = Vec::new();
        for case in suite["cases"].as_array().unwrap() {
            let input = case["in"].as_str().map(str::to_owned).unwrap_or_else(|| {
                let mut token = String::new();
                for part in case["t"].as_array().unwrap() {
                    let alphabet = part["abc"]
                        .as_str()
                        .unwrap_or(suite["rnd_default_abc"].as_str().unwrap())
                        .as_bytes();
                    token.push_str(part["prefix"].as_str().unwrap());
                    for i in 0..part["len"].as_u64().unwrap() as usize {
                        token.push(char::from(alphabet[(i * 7 + 3) % alphabet.len()]));
                    }
                }
                case["template"].as_str().unwrap().replace("{t}", &token)
            });
            let actual = text(&input);
            if actual != case["out"].as_str().unwrap() || text(&actual) != actual {
                failures.push(format!("{}\nactual {actual}\nexpected {}", case["name"], case["out"]));
            }
        }
        for case in suite["deep"].as_array().unwrap() {
            let mut v = case["in"].clone();
            crate::redact::redact_secrets_json(&mut v);
            if v != case["out"] {
                failures.push(format!("{}\nactual {v}\nexpected {}", case["name"], case["out"]));
            }
            crate::redact::redact_secrets_json(&mut v);
            if v != case["out"] {
                failures.push(format!("{} second pass", case["name"]));
            }
        }
        for case in suite["adversarial"].as_array().unwrap() {
            let recipe = &case["recipe"];
            let string = |k: &str| recipe[k].as_str().unwrap_or("");
            let repeat = |part: &str, times: &str| string(part).repeat(recipe[times].as_u64().unwrap_or(0) as usize);
            let input = format!(
                "{}{}{}{}{}",
                string("prefix"),
                repeat("repeat", "times"),
                string("then"),
                repeat("then_repeat", "then_times"),
                string("suffix")
            );
            let started = std::time::Instant::now();
            if let Some(key) = case["under"].as_str() {
                structured(&mut Value::String(input), key);
            } else {
                let _ = text(&input);
            }
            // The upstream budgets target optimized code; unoptimized test builds on shared CI
            // runners are several times slower while staying linear, and a blowup is far larger.
            let budget = case["max_ms"].as_u64().unwrap() * if cfg!(debug_assertions) { 4 } else { 1 };
            if started.elapsed().as_millis() > u128::from(budget) {
                failures.push(format!("{} took {:?}", case["name"], started.elapsed()));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn escaped_credentials_respect_classes_and_remain_idempotent() {
        use crate::redact::{scrub_cow, str_needs_scrub, RedactClasses, RedactOptions, RedactStats};
        let input = r#"[{"note":"bEaR\u0065r AABB1122+CC\/DD==","password":"keepThis123!"}]"#;
        let opts = RedactOptions {
            classes: RedactClasses {
                api_keys: true,
                ..RedactClasses::none()
            },
        };
        // #tooth Only decoding the ordinary value exposes the entire bearer.
        let mut keys: Value = serde_json::from_str(r#"{"ApI\u004bEY":"alpha1234!"}"#).unwrap();
        assert_eq!(fields(&mut keys, ""), 1);
        assert_eq!(keys["ApIKEY"], "[REDACTED:secret]");
        let mut wire = Value::String(input.to_owned());
        assert!(structured(&mut wire, "") > 0);
        assert!(!wire.as_str().unwrap().contains("DD=="));
        for envelope in [input.to_owned(), serde_json::to_string(input).unwrap()] {
            let mut stats = RedactStats::default();
            assert!(str_needs_scrub(&envelope, &opts));
            let out = scrub_cow(&envelope, &opts, &mut stats);
            assert!(!out.contains("AABB") && !out.contains("DD=="), "{out}");
            assert!(out.contains("keepThis123!"));
            assert_eq!((stats.total(), stats.api_keys), (1, 1));
            assert!(!str_needs_scrub(&out, &opts));
        }
    }

    /// #tooth Partially overlapping secret spans fuse into one cut without leaking the trailing secret tail.
    #[test]
    fn tooth_overlapping_secret_spans_redact_entire_union() {
        let input = "Authorization: Bearer abcdefghijklmnop.1234567890:AAbcdefghijklmnopqrstuvwxyz01234567";
        let actual = text(input);
        assert_eq!(actual, "Authorization: Bearer [redacted]");
        assert!(!actual.contains("AAbcdef"), "{actual}");
    }

    /// #tooth Header values that already hold a redaction marker (and no remaining live secret)
    /// keep their class-specific marker and do not re-trigger assignment counts.
    #[test]
    fn tooth_already_redacted_header_preserves_marker_and_counts_zero() {
        let mut already = serde_json::json!({
            "Authorization": "Bearer [REDACTED:github_token]",
            "Cookie": "session=[redacted]"
        });
        assert_eq!(fields(&mut already, ""), 0);
        assert_eq!(already["Authorization"], "Bearer [REDACTED:github_token]");
        assert_eq!(already["Cookie"], "session=[redacted]");

        // Embedded JSON string containing a GitHub token inside a non-Bearer Authorization
        // scheme (`Token <ghp_...>`): `legacy_cow` redacts the token to `[REDACTED:api_key]`,
        // and `secrets::additional` must not overwrite `"Token [REDACTED:api_key]"` with
        // `"[REDACTED:secret]"` or double-count `stats.assignments`.
        let mut session = crate::ir::Session::new("hdr-stats", crate::ir::Harness::Claude);
        let mut msg = crate::ir::Message::new(crate::ir::Role::User);
        let tok = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        msg.content.push(crate::ir::Block::from_text(
            format!(r#"{{"Authorization":"Token {tok}"}}"#).into(),
        ));
        msg.content.push(crate::ir::Block::from_text(
            r#"{"Authorization":"Bearer [REDACTED:github_token]","nested":{"name":"Authorization","value":"Bearer [REDACTED:github_token]"}}"#.into(),
        ));
        session.messages.push(msg);
        let (scrubbed, stats) = crate::redact::redact_with(&session, &crate::redact::RedactOptions::default()).unwrap();
        let out0 = scrubbed.messages[0].content[0].text().unwrap();
        let out1 = scrubbed.messages[0].content[1].text().unwrap();
        assert_eq!(&**out0, r#"{"Authorization":"Token [REDACTED:api_key]"}"#);
        assert_eq!(
            &**out1,
            r#"{"Authorization":"Bearer [REDACTED:github_token]","nested":{"name":"Authorization","value":"Bearer [REDACTED:github_token]"}}"#
        );
        assert_eq!(stats.api_keys, 1);
        assert_eq!(stats.assignments, 0, "must not double-count header after token scrub");
    }
}
