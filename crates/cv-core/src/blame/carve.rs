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

fn hunk(line: &str) -> Option<(Range<usize>, Range<usize>)> {
    let caps = HUNK.captures(line)?;
    let range = |i| -> Option<Range<usize>> {
        let start = caps.get(i)?.as_str().parse::<usize>().ok()?.saturating_sub(1);
        let len = caps.get(i + 1).map_or(Some(1), |n| n.as_str().parse().ok())?;
        Some(start..start.checked_add(len)?)
    };
    Some((range(1)?, range(3)?))
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
    let end = [rest.find('{'), rest.find(" where ")]
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

/// Ignore braces in ordinary quoted literals and comments. Rust uses `'` only for characters
/// and lifetimes; other languages also quote whole strings with it, on one line. An unclosed
/// or unsupported declaration supplies no candidate.
static BRACES_RUST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#""(?:\\.|[^"\\])*"|{CHAR_LITERAL}|//.*|/\*[\s\S]*?\*/|[{{}};]"#
    ))
    .unwrap()
});
static BRACES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#""(?:\\.|[^"\\])*"|{CHAR_LITERAL}|'(?:\\.|[^'\\\n])*'|//.*|/\*[\s\S]*?\*/|[{{}};]"#
    ))
    .unwrap()
});

fn item<'a>(lines: &[&'a str], start: usize, caps: regex::Captures<'a>, rust: bool) -> Option<Item<'a>> {
    let kind = caps.get(1)?.as_str();
    let header = lines[start];
    let key = decl_key(header, &caps)?;
    let braces = if rust { &*BRACES_RUST } else { &*BRACES };
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
        'scan: for (n, line) in lines.iter().enumerate().skip(start) {
            if n > start && depth == 0 && HEADER.is_match(line) {
                return None;
            }
            let mut from = 0;
            for m in braces.find_iter(line) {
                match m.as_str() {
                    "{" => {
                        if depth == 0 {
                            from = m.end()
                        }
                        depth += 1;
                    }
                    "}" => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            body.push_str(&line[from..m.start()]);
                            end = Some(n + 1);
                            break 'scan;
                        }
                    }
                    ";" if depth == 0 => return None,
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
fn containers(lines: &[&str], rust: bool) -> Vec<(Range<usize>, String)> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let caps = HEADER.captures(line)?;
            if !matches!(caps.get(1)?.as_str(), "impl" | "trait" | "class") {
                return None;
            }
            let it = item(lines, i, caps, rust)?;
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
    let lines: Vec<_> = dest.lines().collect();
    let Some(block) = lines.get(range.clone()) else {
        return Ok(None);
    };
    let block: Vec<_> = block.iter().map(|l| norm(l)).collect();
    let exact_ok = block.iter().filter(|l| !l.is_empty()).count() >= 2
        && block.iter().any(|l| l.chars().any(char::is_alphanumeric));
    let enclosing = (0..=range.start)
        .rev()
        .filter_map(|i| item(&lines, i, HEADER.captures(lines[i])?, is_rust(path)))
        .find(|it| it.lines.end >= range.end);
    let target_tokens = enclosing.as_ref().map(|it| tokens(&it.body)).unwrap_or_default();
    let target_key = enclosing
        .as_ref()
        .map(|it| qualify(&containers(&lines, is_rust(path)), it.lines.start, &it.key));
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
        let old_lines: Vec<_> = text.lines().collect();
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
            let outer = containers(&old_lines, is_rust(old));
            for (i, line) in old_lines.iter().enumerate() {
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
                let Some(found) = item(&old_lines, i, caps, is_rust(old)) else {
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
    use super::{creation_hunk, decl_key, item, tokens, HEADER};

    fn first_item(src: &str, rust: bool) -> Option<(String, std::ops::Range<usize>, String)> {
        let lines: Vec<&str> = src.lines().collect();
        let it = item(&lines, 0, HEADER.captures(lines[0])?, rust)?;
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
}
