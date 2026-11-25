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
        r"(fn|def|func|struct|enum|class|trait|impl)(?:<[^>]+>)?\s+(?:\([^)]*\)\s*)?(\w+)\b"
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
    name: &'a str,
    kind: &'a str,
    lines: Range<usize>,
    body: String,
}

// Ignore braces in ordinary quoted literals and line comments. An unclosed or
// unsupported declaration supplies no candidate.
static BRACES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|//.*|/\*[\s\S]*?\*/|[{};]"#).unwrap());

fn item<'a>(lines: &[&'a str], start: usize, caps: regex::Captures<'a>) -> Option<Item<'a>> {
    let kind = caps.get(1)?.as_str();
    let name = caps.get(2)?.as_str();
    let header = lines[start];
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
            for m in BRACES.find_iter(line) {
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
        name,
        kind,
        lines: start..end,
        body: body.trim().to_owned(),
    })
}

// Token boundaries survive formatting changes. Quoted strings retain their whitespace.
static TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|\w+|::|->|=>|==|!=|<=|>=|&&|\|\||\S"#).unwrap());

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

pub(super) fn find(root: &Path, sha: &str, diff: &str) -> Result<Option<Relocation>> {
    let mut hunks = diff.lines().filter_map(hunk);
    let Some((before, range)) = hunks.next() else {
        return Ok(None);
    };
    if !before.is_empty() || range.is_empty() || hunks.next().is_some() {
        return Ok(None);
    }
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
            "--diff-filter=M",
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
        .filter_map(|i| item(&lines, i, HEADER.captures(lines[i])?))
        .find(|it| it.lines.end >= range.end);
    let target_tokens = enclosing.as_ref().map(|it| tokens(&it.body)).unwrap_or_default();
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
        if let Some(target) = &enclosing {
            for (i, line) in old_lines.iter().enumerate() {
                let Some(caps) = HEADER.captures(line) else { continue };
                if caps.get(1).is_none_or(|k| k.as_str() != target.kind) {
                    continue;
                }
                let same = caps.get(2).is_some_and(|n| n.as_str() == target.name);
                if same {
                    names += 1;
                    if names > 1 {
                        return Ok(None);
                    }
                }
                if similar.len() >= 2 {
                    continue;
                }
                let Some(found) = item(&old_lines, i, caps) else {
                    continue;
                };
                if target.body.is_empty() || found.body.is_empty() {
                    continue;
                }
                let found_tokens = tokens(&found.body);
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
    // deletion hunk too. Its re-addition makes the cross-file ancestry uncertain.
    let retained = patch.lines().filter_map(|line| line.strip_prefix('+')).any(|line| {
        enclosing.as_ref().is_some_and(|target| {
            HEADER
                .captures(line)
                .is_some_and(|c| &c[1] == target.kind && &c[2] == target.name)
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
