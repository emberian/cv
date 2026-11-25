//! Bridge a line-log creation hunk to one removed block in the commit's parent.
use super::{git_stdout, run_git, Relocation};
use anyhow::Result;
use regex::Regex;
use std::ops::Range;
use std::path::Path;
use std::sync::LazyLock;

static HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^\s*(?:(?:pub(?:\([^)]*\))?|async|unsafe|const|export|abstract|public|private|static)\s+)*",
        r"(fn|def|func|struct|enum|class|trait|impl)(?:<(?:[^<>]|<[^<>]*>)+>)?\s+(?:\([^)]*\)\s*)?(\w+)\b"
    ))
    .unwrap()
});
static HUNK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").unwrap());
static WHERE_CLAUSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\swhere(?:\s|$)").unwrap());

pub(super) fn hunk(line: &str) -> Option<(Range<usize>, Range<usize>)> {
    let caps = HUNK.captures(line)?;
    let range = |i| -> Option<Range<usize>> {
        let start = caps.get(i)?.as_str().parse::<usize>().ok()?.saturating_sub(1);
        let len = caps.get(i + 1).map_or(Some(1), |n| n.as_str().parse().ok())?;
        Some(start..start.checked_add(len)?)
    };
    Some((range(1)?, range(3)?))
}

/// Map a 1-based inclusive line range `(lo, hi)` from a post-image (`+`) revision backward across
/// a unified diff to its 1-based inclusive pre-image (`-`) range.
pub(super) fn map_range_new_to_old(range: (usize, usize), diff: &str) -> (usize, usize) {
    let hunks: Vec<(Range<usize>, Range<usize>)> = diff.lines().filter_map(hunk).collect();
    if hunks.is_empty() {
        return range;
    }
    let lo0 = range.0.saturating_sub(1);
    let hi0 = range.1.max(range.0);

    let map_point = |p: usize, is_end: bool| -> usize {
        let mut delta: isize = 0;
        for (old, new) in &hunks {
            if p < new.start {
                break;
            }
            if p <= new.end && (p < new.end || (is_end && p == new.end && !new.is_empty())) {
                return if is_end { old.end } else { old.start };
            }
            delta += new.end as isize - old.end as isize;
        }
        (p as isize - delta).max(0) as usize
    };

    let old_lo0 = map_point(lo0, false);
    let old_hi0 = map_point(hi0, true);
    let lo1 = old_lo0 + 1;
    let hi1 = old_hi0.max(lo1);
    (lo1, hi1)
}

struct Item<'a> {
    key: String,
    kind: &'a str,
    lines: Range<usize>,
    body: String,
}

/// The text from `s`'s start through its balanced `<…>` run, or `None` when unbalanced.
fn balanced_angles(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => (),
        }
    }
    None
}

/// The last `::` segment of a path, generics kept but whitespace dropped (`fmt::Display` →
/// `Display`, `From< A >` → `From<A>`).
fn last_segment(path: &str) -> String {
    let compact: String = path.split_whitespace().collect();
    let head = compact.find('<').unwrap_or(compact.len());
    let start = compact[..head].rfind("::").map_or(0, |i| i + 2);
    compact[start..].to_owned()
}

/// What identifies a declaration among its siblings: the name, or for an `impl` its trait and
/// self type (`Display for Widget`), or the bare self type of an inherent `impl`. Two types
/// implementing one trait are different items.
fn decl_key(line: &str, caps: &regex::Captures<'_>) -> Option<String> {
    let kind = caps.get(1)?;
    if kind.as_str() != "impl" {
        return Some(caps.get(2)?.as_str().to_owned());
    }
    let mut rest = line[kind.end()..].trim_start();
    if rest.starts_with('<') {
        rest = rest[balanced_angles(rest)?..].trim_start();
    }
    let end = [rest.find('{'), WHERE_CLAUSE.find(rest).map(|m| m.start())]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(rest.len());
    let rest = rest[..end].trim().trim_start_matches('!');
    let self_type = |t: &str| {
        let t = last_segment(t);
        let base = t.find('<').unwrap_or(t.len());
        t[..base].to_owned()
    };
    Some(match rest.split_once(" for ") {
        Some((tr, ty)) => format!("{} for {}", last_segment(tr), self_type(ty)),
        None => self_type(rest),
    })
}

/// Character literals, strictly: one character or one escape. A Rust lifetime (`'a`,
/// `'static`, `'_`) is not one, so `<'a, 'b>` never pairs its quotes across the signature.
const CHAR_LITERAL: &str = r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]{1,6}\}|.)|[^'\\\n])'";
static CHAR_LIT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!("^{CHAR_LITERAL}")).unwrap());

/// Mask comments, quoted strings, character literals, and (when `rust`) raw string literals
/// (`r#"..."#`, `br#"..."#`, `cr#"..."#`) and nested `/* ... */` comments across the whole file,
/// replacing non-newline bytes inside them with spaces so line numbers and byte offsets of real
/// `{`, `}`, and `;` delimiters remain 1:1 with `src`.
fn mask_for_braces(src: &str, rust: bool) -> String {
    let b = src.as_bytes();
    let len = b.len();
    let mut out = b.to_vec();
    let blank = |out: &mut [u8], range: Range<usize>| {
        for slot in &mut out[range] {
            if *slot != b'\n' && *slot != b'\r' {
                *slot = b' ';
            }
        }
    };
    let is_ident_prev = |idx: usize| -> bool {
        idx > 0 && (b[idx - 1].is_ascii_alphanumeric() || b[idx - 1] == b'_' || b[idx - 1] == b'\'')
    };

    let mut i = 0usize;
    while i < len {
        // Line comment `// ...`
        if i + 1 < len && b[i] == b'/' && b[i + 1] == b'/' {
            let start = i;
            i += 2;
            while i < len && b[i] != b'\n' {
                i += 1;
            }
            blank(&mut out, start..i);
            continue;
        }
        // Block comment `/* ... */` (nested in Rust)
        if i + 1 < len && b[i] == b'/' && b[i + 1] == b'*' {
            let start = i;
            i += 2;
            let mut depth = 1usize;
            while i < len && depth > 0 {
                if rust && i + 1 < len && b[i] == b'/' && b[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if i + 1 < len && b[i] == b'*' && b[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            blank(&mut out, start..i);
            continue;
        }
        // Rust raw string literal: `r#"..."#`, `br#"..."#`, `cr#"..."#`
        if rust && !is_ident_prev(i) {
            let r_pos = if b[i] == b'r' {
                Some(i)
            } else if (b[i] == b'b' || b[i] == b'c') && i + 1 < len && b[i + 1] == b'r' {
                Some(i + 1)
            } else {
                None
            };
            if let Some(rp) = r_pos {
                let mut hashes = 0usize;
                let mut k = rp + 1;
                while k < len && b[k] == b'#' {
                    hashes += 1;
                    k += 1;
                }
                if hashes < 256 && k < len && b[k] == b'"' {
                    let start = i;
                    k += 1;
                    while k < len {
                        if b[k] == b'"' && k + 1 + hashes <= len && b[k + 1..k + 1 + hashes].iter().all(|&c| c == b'#')
                        {
                            k += 1 + hashes;
                            break;
                        }
                        k += 1;
                    }
                    blank(&mut out, start..k);
                    i = k;
                    continue;
                }
            }
        }
        // Ordinary double-quoted string `"..."` (may span lines)
        if b[i] == b'"' {
            let start = i;
            i += 1;
            while i < len {
                if b[i] == b'\\' {
                    i = (i + 2).min(len);
                } else if b[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            blank(&mut out, start..i);
            continue;
        }
        // Single-quoted character literal (Rust) or single-line string (non-Rust)
        if b[i] == b'\'' {
            if rust {
                if let Some(m) = CHAR_LIT_RE.find(&src[i..]) {
                    blank(&mut out, i..i + m.end());
                    i += m.end();
                    continue;
                }
            } else {
                let start = i;
                let mut k = i + 1;
                let mut closed = false;
                while k < len && b[k] != b'\n' {
                    if b[k] == b'\\' {
                        k = (k + 2).min(len);
                    } else if b[k] == b'\'' {
                        k += 1;
                        closed = true;
                        break;
                    } else {
                        k += 1;
                    }
                }
                if closed {
                    blank(&mut out, start..k);
                    i = k;
                    continue;
                }
            }
        }
        // Advance to next UTF-8 char boundary so `&src[i..]` is always valid UTF-8.
        i += src[i..].chars().next().map_or(1, char::len_utf8);
    }
    String::from_utf8(out).expect("masking preserves valid UTF-8")
}

fn item<'a>(lines: &[&'a str], masked_lines: &[&str], start: usize, caps: regex::Captures<'a>) -> Option<Item<'a>> {
    let kind = caps.get(1)?.as_str();
    let header = lines[start];
    let key = decl_key(header, &caps)?;
    let (end, body) = if matches!(&caps[1], "def" | "class") && header.contains(':') && !header.contains('{') {
        let indent = header.len() - header.trim_start().len();
        let mut tail = lines[start + 1..]
            .iter()
            .take_while(|l| l.trim().is_empty() || l.len() - l.trim_start().len() > indent)
            .count();
        while tail > 0 && lines[start + tail].trim().is_empty() {
            tail -= 1;
        }
        (
            start + 1 + tail,
            format!(
                "{}\n{}",
                header.rsplit_once(':')?.1,
                lines[start + 1..start + 1 + tail].join("\n")
            ),
        )
    } else {
        let mut depth = 0usize;
        let mut body = String::new();
        let mut end = None;
        'scan: for (n, (line, masked)) in lines.iter().zip(masked_lines.iter()).enumerate().skip(start) {
            if n > start && depth == 0 && HEADER.is_match(line) && !masked.trim().is_empty() {
                return None;
            }
            let mut from = 0;
            for (pos, ch) in masked.bytes().enumerate() {
                match ch {
                    b'{' => {
                        if depth == 0 {
                            from = pos + 1;
                        }
                        depth += 1;
                    }
                    b'}' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            body.push_str(&line[from..pos]);
                            end = Some(n + 1);
                            break 'scan;
                        }
                    }
                    b';' if depth == 0 => return None,
                    _ => (),
                }
            }
            if depth > 0 {
                body.push_str(&line[from..]);
                body.push('\n');
            }
        }
        (end?, body)
    };
    Some(Item {
        key,
        kind,
        lines: start..end,
        body: body.trim().to_owned(),
    })
}

/// The `impl`, `trait` and `class` blocks of a file: their lines and keys.
fn containers(lines: &[&str], masked_lines: &[&str]) -> Vec<(Range<usize>, String)> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            if masked_lines.get(i).is_some_and(|m| m.trim().is_empty()) {
                return None;
            }
            let caps = HEADER.captures(line)?;
            if !matches!(caps.get(1)?.as_str(), "impl" | "trait" | "class") {
                return None;
            }
            let it = item(lines, masked_lines, i, caps)?;
            Some((it.lines, it.key))
        })
        .collect()
}

/// `key` qualified by the innermost container strictly around line `at`, so the `fmt` of
/// `Display for Widget` and the `fmt` of `Display for Gadget` are different items.
fn qualify(containers: &[(Range<usize>, String)], at: usize, key: &str) -> String {
    containers
        .iter()
        .filter(|(r, _)| r.start < at && at < r.end)
        .min_by_key(|(r, _)| r.len())
        .map_or_else(|| key.to_owned(), |(_, outer)| format!("{outer}::{key}"))
}

// Token boundaries survive formatting changes. Quoted strings and character literals retain
// their whitespace; a lifetime is a `'` token and a word.
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#""(?:\\.|[^"\\])*"|{CHAR_LITERAL}|\w+|::|->|=>|==|!=|<=|>=|&&|\|\||\S"#
    ))
    .unwrap()
});

/// Bodies whose token counts differ by more than this factor are never similar enough: the
/// check skips the O(n·m) similarity measure for them.
const MAX_TOKEN_RATIO: usize = 3;

fn is_rust(path: &str) -> bool {
    path.ends_with(".rs")
}

fn tokens(text: &str) -> Vec<&str> {
    TOKEN.find_iter(text).map(|m| m.as_str()).collect()
}

fn norm(line: &str) -> String {
    tokens(line).join(" ")
}

struct Candidate {
    path: String,
    range: Range<usize>,
}

/// The one pure-addition hunk of `diff` the walk follows. A single addition hunk is taken as is;
/// among several, exactly one must overlap `query` (1-based inclusive lines of the tracked
/// range), or the walk stops rather than guess.
fn creation_hunk(diff: &str, query: (usize, usize)) -> Option<Range<usize>> {
    let added: Vec<Range<usize>> = diff
        .lines()
        .filter_map(hunk)
        .filter(|(before, after)| before.is_empty() && !after.is_empty())
        .map(|(_, after)| after)
        .collect();
    if let [only] = added.as_slice() {
        return Some(only.clone());
    }
    let want = query.0.saturating_sub(1)..query.1.max(query.0);
    let mut covering = added.into_iter().filter(|r| r.start < want.end && want.start < r.end);
    let found = covering.next()?;
    covering.next().is_none().then_some(found)
}

pub(super) fn find(root: &Path, sha: &str, diff: &str, query: (usize, usize)) -> Result<Option<Relocation>> {
    // A hunk that also removes lines is an edit, not a carve: only pure additions bridge.
    if diff
        .lines()
        .filter_map(hunk)
        .any(|(before, after)| !before.is_empty() && !after.is_empty())
    {
        return Ok(None);
    }
    let Some(range) = creation_hunk(diff, query) else {
        return Ok(None);
    };
    let Some(path) = diff.lines().find_map(|l| l.strip_prefix("+++ b/")) else {
        return Ok(None);
    };
    let path = path.trim_end_matches('\t');
    let parents = git_stdout(root, &["rev-list", "--parents", "-n1", sha])?;
    let mut parents = parents.split_whitespace().skip(1);
    let Some(parent) = parents.next() else { return Ok(None) };
    if parents.next().is_some() {
        return Ok(None);
    }
    let changed = run_git(
        root,
        &[
            "diff",
            "--no-renames",
            // `D` covers the split refactor: `foo.rs` deleted while `foo/a.rs` and `foo/b.rs`
            // are created in the same commit.
            "--diff-filter=MD",
            "--name-only",
            "-z",
            parent,
            sha,
            "--",
        ],
    )?;
    let Ok(paths) = std::str::from_utf8(&changed.stdout) else {
        return Ok(None);
    };
    if !changed.status.success() || !paths.split('\0').any(|p| !p.is_empty() && p != path) {
        return Ok(None);
    }
    let dest = git_stdout(root, &["show", &format!("{sha}:{path}")])?;
    let dest_masked = mask_for_braces(&dest, is_rust(path));
    let lines: Vec<_> = dest.lines().collect();
    let masked_lines: Vec<_> = dest_masked.lines().collect();
    let Some(block) = lines.get(range.clone()) else {
        return Ok(None);
    };
    let block: Vec<_> = block.iter().map(|l| norm(l)).collect();
    let exact_ok = block.iter().filter(|l| !l.is_empty()).count() >= 2
        && block.iter().any(|l| l.chars().any(char::is_alphanumeric));
    let enclosing = (0..=range.start)
        .rev()
        .filter(|&i| !masked_lines.get(i).is_some_and(|m| m.trim().is_empty()))
        .filter_map(|i| item(&lines, &masked_lines, i, HEADER.captures(lines[i])?))
        .find(|it| it.lines.end >= range.end);
    let target_tokens = enclosing.as_ref().map(|it| tokens(&it.body)).unwrap_or_default();
    let target_key = enclosing
        .as_ref()
        .map(|it| qualify(&containers(&lines, &masked_lines), it.lines.start, &it.key));
    let mut exact = Vec::new();
    let mut similar = Vec::new();
    let mut names = 0;
    for old in paths.split('\0').filter(|p| !p.is_empty() && *p != path) {
        let blob = run_git(root, &["cat-file", "blob", &format!("{parent}:{old}")])?;
        if !blob.status.success() {
            return Ok(None);
        }
        let Ok(text) = std::str::from_utf8(&blob.stdout) else {
            continue;
        };
        if text.contains('\0') {
            continue;
        }
        let old_masked = mask_for_braces(text, is_rust(old));
        let old_lines: Vec<_> = text.lines().collect();
        let old_masked_lines: Vec<_> = old_masked.lines().collect();
        let normalized: Vec<_> = old_lines.iter().map(|l| norm(l)).collect();
        let candidate = |range| Candidate {
            path: old.into(),
            range,
        };
        if exact_ok && exact.len() < 2 {
            for (i, _) in normalized.windows(block.len()).enumerate().filter(|(_, w)| *w == block) {
                exact.push(candidate(i..i + block.len()));
                if exact.len() == 2 {
                    break;
                }
            }
        }
        if let (Some(target), Some(target_key)) = (&enclosing, &target_key) {
            let outer = containers(&old_lines, &old_masked_lines);
            for (i, line) in old_lines.iter().enumerate() {
                if old_masked_lines.get(i).is_some_and(|m| m.trim().is_empty()) {
                    continue;
                }
                let Some(caps) = HEADER.captures(line) else { continue };
                if caps.get(1).is_none_or(|k| k.as_str() != target.kind) {
                    continue;
                }
                let Some(bare) = decl_key(line, &caps) else { continue };
                let same = qualify(&outer, i, &bare) == *target_key;
                // An `impl` is identified by its key, and a method by its container's, so another
                // key (`Display for Gadget`, `Display for Gadget::fmt`) is never the ancestor.
                // Inherent `impl Foo` blocks repeat legitimately; body similarity decides
                // among them, and two similar ones refuse below.
                let is_impl = target.kind == "impl";
                if !same && (is_impl || bare == target.key) {
                    continue;
                }
                if same && !is_impl {
                    names += 1;
                    if names > 1 {
                        return Ok(None);
                    }
                }
                if similar.len() >= 2 {
                    continue;
                }
                let Some(found) = item(&old_lines, &old_masked_lines, i, caps) else {
                    continue;
                };
                if target.body.is_empty() || found.body.is_empty() {
                    continue;
                }
                let found_tokens = tokens(&found.body);
                let (short, long) = (
                    target_tokens.len().min(found_tokens.len()),
                    target_tokens.len().max(found_tokens.len()),
                );
                if long > short.saturating_mul(MAX_TOKEN_RATIO) {
                    continue;
                }
                let normalized_len = |tokens: &[&str]| tokens.iter().map(|t| t.chars().count()).sum::<usize>();
                let threshold = if normalized_len(&target_tokens).min(normalized_len(&found_tokens)) < 20 {
                    0.95
                } else {
                    0.82
                };
                if strsim::generic_jaro_winkler(&target_tokens, &found_tokens) >= threshold {
                    similar.push((same, candidate(found.lines)));
                }
            }
        }
    }
    if similar.len() > 1 {
        return Ok(None);
    }
    let (candidate, method) = if exact.len() == 1 {
        (exact.pop().unwrap(), "exact_block")
    } else if let [(true, _)] = similar.as_slice() {
        (similar.pop().unwrap().1, "item_body")
    } else {
        return Ok(None);
    };
    let patch = git_stdout(
        root,
        &[
            "--literal-pathspecs",
            "diff",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "-U0",
            parent,
            sha,
            "--",
            &candidate.path,
        ],
    )?;
    let removed = patch
        .lines()
        .filter_map(hunk)
        .any(|(r, _)| r.start <= candidate.range.start && r.end >= candidate.range.end);
    // Moving the same declaration elsewhere within the source file can leave a
    // deletion hunk too. Its re-addition makes the cross-file ancestry uncertain. Added lines
    // carry no enclosing block, so the unqualified key decides (refusing more, never less).
    let retained = patch.lines().filter_map(|line| line.strip_prefix('+')).any(|line| {
        enclosing.as_ref().is_some_and(|target| {
            HEADER
                .captures(line)
                .is_some_and(|c| &c[1] == target.kind && decl_key(line, &c).is_some_and(|k| k == target.key))
        })
    });
    if !removed || retained {
        return Ok(None);
    }
    Ok(Some(Relocation {
        commit: sha.into(),
        parent: parent.into(),
        path: candidate.path,
        lines: (candidate.range.start + 1, candidate.range.end),
        method: method.into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::{creation_hunk, decl_key, item, map_range_new_to_old, mask_for_braces, tokens, HEADER};

    fn first_item(src: &str, rust: bool) -> Option<(String, std::ops::Range<usize>, String)> {
        let masked = mask_for_braces(src, rust);
        let lines: Vec<&str> = src.lines().collect();
        let masked_lines: Vec<&str> = masked.lines().collect();
        let it = item(&lines, &masked_lines, 0, HEADER.captures(lines[0])?)?;
        Some((it.key, it.lines, it.body))
    }

    #[test]
    fn lifetimes_never_pair_into_a_character_literal() {
        // #tooth `'a … 'b` once read as one quoted literal, swallowing the `(` … `{` between them.
        let pick = "let pick = |s: &'a str| if s.is_empty() { '-' } else { '{' };";
        let src = format!("pub fn merge<'a, 'b>(left: &'a str, right: &'b str) -> (&'a str, &'b str) {{\n    {pick}\n    (left, right)\n}}\nfn next() {{}}\n");
        let (key, lines, body) = first_item(&src, true).unwrap();
        assert_eq!((key.as_str(), lines), ("merge", 0..4));
        assert_eq!(body, format!("{pick}\n    (left, right)"));
        let toks = tokens("fn f<'a>(x: &'a str) -> char { '\\u{7b}' }");
        assert!(toks.contains(&"'\\u{7b}'") && toks.contains(&"a"), "{toks:?}");
        // Elsewhere a single-quoted string still hides its braces.
        let js = "class F {\n  s = '}{';\n}\n";
        assert_eq!(first_item(js, false).unwrap().1, 0..3);
    }

    #[test]
    fn impl_keys_name_the_trait_and_the_self_type() {
        // #tooth Two types implementing one trait were one name (`Display`), so either refused.
        let key = |line: &str| decl_key(line, &HEADER.captures(line).unwrap()).unwrap();
        assert_eq!(key("impl fmt::Display for Widget {"), "Display for Widget");
        assert_eq!(
            key("impl<'a, T: Into<String>> From<T> for Wrapper<'a> where T: Clone {"),
            "From<T> for Wrapper"
        );
        assert_eq!(key("impl Widget {"), "Widget");
        assert_eq!(key("impl<T> crate::Gadget<T> {"), "Gadget");
        assert_eq!(key("pub fn widget() {"), "widget");
    }

    #[test]
    fn creation_hunk_selects_the_one_addition_covering_the_query() {
        // #tooth Two addition hunks: the query picks one; a query touching both, or neither, refuses.
        let diff = "+++ b/x.rs\n@@ -3,0 +4,5 @@\n+a\n@@ -20,0 +26,4 @@\n+b\n";
        assert_eq!(creation_hunk(diff, (5, 6)), Some(3..8));
        assert_eq!(creation_hunk(diff, (27, 29)), Some(25..29));
        assert_eq!(creation_hunk(diff, (6, 27)), None);
        assert_eq!(creation_hunk(diff, (12, 14)), None);
        assert_eq!(creation_hunk("@@ -0,0 +1,9 @@\n", (40, 41)), Some(0..9));
    }

    #[test]
    fn multiline_comments_raw_strings_where_newline_and_diff_range_tracking_hold() {
        // #tooth Multi-line `/* ... { ... */`, Rust raw strings `r#" ... } ... "#`, and
        // `impl ... where\n` must never confuse brace depth or self-type extraction, and
        // `map_range_new_to_old` tracks line shifts back to the creation commit.
        let src = concat!(
            "impl fmt::Display for Widget where\n",
            "    Widget: Clone,\n",
            "{\n",
            "    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {\n",
            "        /* multi-line comment with unmatched {\n",
            "           and nested /* } */ still open {\n",
            "        */\n",
            "        let raw = r#\"unmatched }\n",
            "        still inside raw string }};\"#;\n",
            "        write!(f, \"{raw}\")\n",
            "    }\n",
            "}\n",
            "fn trailing() {}\n",
        );
        let (key, lines, _) = first_item(src, true).unwrap();
        assert_eq!(key, "Display for Widget");
        assert_eq!(lines, 0..12);

        let diff = "@@ -0,0 +1,22 @@\n+header\n@@ -4,5 +26,7 @@\n";
        assert_eq!(map_range_new_to_old((26, 32), diff), (4, 8));
        assert_eq!(map_range_new_to_old((48, 51), "@@ -0,0 +1,22 @@\n"), (26, 29));
    }
}
