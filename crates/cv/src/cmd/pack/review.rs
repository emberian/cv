//! Blind review reads checkout artifacts. Session material only selects paths.
use anyhow::Result;
use cv_core::ir::Harness;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) fn runner(path: &str) -> bool {
    let name = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(
        name,
        "conftest.py" | "pytest.ini" | "tox.ini" | "noxfile.py" | "sitecustomize.py" | "run-tests.sh" | "run_tests.py"
    ) || name.starts_with("jest.config.")
        || name.starts_with("vitest.config.")
}

fn git(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .current_dir(cwd)
        .arg("--literal-pathspecs")
        .args(args)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "could not read review artifacts from {}",
        cwd.display()
    );
    Ok(out.stdout)
}

/// Scrub the review request (the pack header): reasoning blocks go, and so does any metadata line
/// naming an author, model, or verdict badge. Never applied to checkout content.
fn clean(text: &str) -> String {
    use std::sync::LazyLock;
    static THOUGHT: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?is)<thinking\b[^>]*>.*?(?:</thinking>|\z)").unwrap());
    static CUE: LazyLock<regex::Regex> = LazyLock::new(|| {
        let names = Harness::ALL.map(|h| regex::escape(h.as_str())).join("|");
        regex::Regex::new(&format!(
            r"(?i:\b(?:{names}|gpt|anthropic|openai|llama|sonnet|haiku)(?:[- .][0-9][\w.-]*)?\b)|\bPASS(?:ED)?\b"
        ))
        .unwrap()
    });
    static META: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)^\s*(?:[-*>]\s*)?(?:\*\*)?(?:author|authored[- ]by|model|reviewer|agent|generated[- ]by|co-?authored[- ]by|signed-off-by|status|verdict|badge|result)s?(?:\*\*)?\s*[:=]|^\s*(?:!\[[^\]]*\]\([^)]*\)\s*)+$|^\s*\[!\[")
            .unwrap()
    });
    let text = THOUGHT.replace_all(text, "");
    text.lines()
        .filter(|line| !(META.is_match(line) || CUE.is_match(line) && line.split_whitespace().count() <= 3))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Remove comments while retaining quoted strings, including URLs and regex fragments.
/// Old and new source each have their own multiline-comment state.
fn source_line(line: &str, block: &mut bool, hash_comments: bool) -> String {
    let mut code = String::new();
    let mut chars = line.chars().peekable();
    let mut quote = None;
    while let Some(c) = chars.next() {
        if *block {
            if c == '*' && chars.next_if_eq(&'/').is_some() {
                *block = false;
            }
        } else if let Some(q) = quote {
            code.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    code.push(next);
                }
            } else if c == q {
                quote = None;
            }
        } else if c == '/' && chars.next_if_eq(&'*').is_some() {
            *block = true;
        } else if c == '/' && !hash_comments && chars.peek() == Some(&'/') || c == '#' && hash_comments {
            break;
        } else {
            if matches!(c, '"' | '\'' | '`') {
                quote = Some(c)
            }
            code.push(c);
        }
    }
    code
}

fn artifact(diff: &str, path: &str) -> String {
    let hash_comments = matches!(
        Path::new(path).extension().and_then(|e| e.to_str()),
        Some("py" | "sh" | "bash" | "rb" | "toml" | "yaml" | "yml" | "pl" | "r")
    );
    let mut comment = [false; 2];
    let mut rows = Vec::new();
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_hunk = false;
            comment = [false; 2];
        }
        if line.starts_with("@@ ") {
            in_hunk = true;
            comment = [false; 2];
            continue;
        }
        if !in_hunk {
            continue;
        }
        let Some((prefix, body)) = line.split_at_checked(1) else {
            continue;
        };
        let code = match prefix {
            " " => {
                source_line(body, &mut comment[0], hash_comments);
                source_line(body, &mut comment[1], hash_comments);
                continue;
            }
            "+" | "-" => source_line(body, &mut comment[usize::from(prefix == "+")], hash_comments),
            _ => continue,
        };
        if !code.trim().is_empty() {
            rows.push(format!("{prefix}{}", code.trim_end()))
        }
    }
    rows.join("\n")
}

pub(super) fn bundle(
    task: &str,
    selections: &BTreeMap<PathBuf, Option<BTreeSet<String>>>,
    all: bool,
) -> Result<String> {
    let mut out = format!("# Review\n\n{}\n", clean(task));
    let mut count = 0;
    for (cwd, paths) in selections {
        let root = PathBuf::from(String::from_utf8(git(cwd, &["rev-parse", "--show-toplevel"])?)?.trim_end());
        let paths = match paths {
            Some(set) => set
                .iter()
                .filter_map(|s| Path::new(s).strip_prefix(&root).ok()?.to_str().map(str::to_owned))
                .collect(),
            None => git(
                &root,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--name-only",
                    "-z",
                    "HEAD",
                    "--",
                ],
            )?
            .split(|&b| b == 0)
            .filter_map(|s| std::str::from_utf8(s).ok())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect::<BTreeSet<_>>(),
        };
        for path in paths {
            if !all && runner(&path) {
                continue;
            }
            let diff = git(
                &root,
                &[
                    "diff",
                    "--no-renames",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--unified=3",
                    "HEAD",
                    "--",
                    &path,
                ],
            )?;
            let text = artifact(std::str::from_utf8(&diff).unwrap_or(""), &path);
            if !text.is_empty() {
                writeln!(out, "\n## {}\n\n{text}", cv_core::sanitize::sanitize_line(&path))?;
                count += 1;
            }
        }
    }
    if count == 0 {
        out.push_str("\nNo reviewable tracked changes in the selected checkout.\n");
    }
    Ok(out)
}
