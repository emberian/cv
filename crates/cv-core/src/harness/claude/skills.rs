//! What a derived Claude session restores of the skills its source invoked (`--skills`).
//!
//! Claude Code (traced in 2.1.302) records each skill a session invokes with its text as it was
//! rendered then. Every compaction writes all of them back as one `invoked_skills` attachment, and
//! `--resume` rebuilds the store from the `invoked_skills` records after the last compact boundary.
//! Nothing else forgets an entry, so a skill invoked once is restored at every later compaction
//! with its old text, even after its SKILL.md changed or the skill was removed.
//!
//! A prune, rewind or port can drop those entries from the session it writes: `drop` drops every
//! one, `drop-stale` the ones whose file (the skill's `SKILL.md`, or the legacy `commands/<name>.md`
//! a slash command became) is gone or no longer matches the stored text. A rewritten record loses
//! its stored `rendered*` fields, so Claude Code renders it again from the kept list (an empty list
//! renders nothing). A dropped skill stays invocable: the model gets its current text when it uses
//! it again. Only the new session is rewritten, never the source.

use super::replay::{copy, rows};
use crate::harness::store::{
    bytes::{ByteSource, ByteStage},
    document::range::{Range, RangeSource, Validation},
};
use anyhow::Result;
use serde_json::Value;
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Which restored skills a derived session keeps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SkillsPolicy {
    /// Every entry, as recorded (the source's behaviour).
    #[default]
    Keep,
    /// No entry: the resumed session restores no skill.
    Drop,
    /// Drop entries whose file is gone or differs from the stored text: `skills/<name>/SKILL.md`, or the
    /// legacy `commands/<name>.md` a slash command became. Bundled and plugin skills have no file
    /// to compare and are kept.
    DropStale,
}

impl SkillsPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillsPolicy::Keep => "keep",
            SkillsPolicy::Drop => "drop",
            SkillsPolicy::DropStale => "drop-stale",
        }
    }
}

impl std::str::FromStr for SkillsPolicy {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "keep" => SkillsPolicy::Keep,
            "drop" => SkillsPolicy::Drop,
            "drop-stale" => SkillsPolicy::DropStale,
            _ => anyhow::bail!("skills policy {s:?}: expected keep, drop or drop-stale"),
        })
    }
}

/// What the policy did to one session.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SkillsReport {
    pub policy: &'static str,
    /// `invoked_skills` records rewritten.
    pub records: usize,
    /// Entries dropped, and the characters of stored skill text they held.
    pub dropped: usize,
    pub dropped_chars: usize,
    /// Entries kept in the records the policy read.
    pub kept: usize,
}

impl SkillsReport {
    /// The default policy leaves both transcript and report output unchanged.
    pub fn is_keep(&self) -> bool {
        self.policy.is_empty() || self.policy == "keep"
    }
}

/// How Claude Code opens a skill's stored text, and its per-skill truncation marker.
const BASE_DIR: &str = "Base directory for this skill: ";
const TRUNCATED: &str =
    "\n\n[... skill content truncated for compaction; use Read on the skill path if you need the full text]";
/// The record keys that hold an attachment's stored rendering.
const RENDERED: [&str; 4] = [
    "rendered",
    "renderedRole",
    "renderedInHumanTurn",
    "renderedBesideToolResult",
];

/// The Claude config dir a session file belongs to: `<config>/projects/<project>/<id>.jsonl`.
pub fn config_dir_of(session: &Path) -> Option<PathBuf> {
    let projects = session
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "projects"))?;
    projects.parent().map(Path::to_path_buf)
}

/// Rewrite the main-thread `invoked_skills` records of a session's bytes. `config` is the Claude
/// config dir whose `skills/` holds user skills; without it they cannot be judged and are kept.
pub(crate) fn apply(
    bytes: ByteSource,
    policy: SkillsPolicy,
    config: Option<&Path>,
) -> Result<(ByteSource, SkillsReport)> {
    let mut report = SkillsReport {
        policy: policy.as_str(),
        ..SkillsReport::default()
    };
    if policy == SkillsPolicy::Keep {
        return Ok((bytes, report));
    }
    let bytes = Arc::new(bytes);
    let source = RangeSource::private(&bytes)?;
    let mut output = ByteStage::new();
    rows(&bytes, |row| {
        if !restore_record(&source, row)? {
            return copy(&source, row, &mut output);
        }
        let mut record: Value = source.materialize(row)?;
        let cwd = record.get("cwd").and_then(Value::as_str).map(PathBuf::from);
        let Some(skills) = record.pointer_mut("/attachment/skills").and_then(Value::as_array_mut) else {
            return copy(&source, row, &mut output);
        };
        let before = skills.len();
        skills.retain(|entry| {
            let keep = policy == SkillsPolicy::DropStale && current(entry, config, cwd.as_deref());
            if !keep {
                report.dropped_chars += entry
                    .get("content")
                    .and_then(Value::as_str)
                    .map_or(0, |c| c.chars().count());
            }
            keep
        });
        report.kept += skills.len();
        if skills.len() == before {
            return copy(&source, row, &mut output);
        }
        report.dropped += before - skills.len();
        report.records += 1;
        if let Some(fields) = record.as_object_mut() {
            for key in RENDERED {
                fields.remove(key);
            }
        }
        serde_json::to_writer(&mut output, &record)?;
        output.write_all(b"\n")?;
        Ok(())
    })?;
    Ok((output.finish()?, report))
}

/// Whether [`apply`] will rewrite `message`'s carried main-thread `invoked_skills` record (and
/// therefore strip its `rendered*` fields), so the fidelity projection and the byte rewriter agree.
pub(crate) fn strips_rendered(
    message: &crate::ir::Message,
    policy: SkillsPolicy,
    config: Option<&Path>,
    effective_cwd: Option<&Path>,
) -> bool {
    if policy == SkillsPolicy::Keep || message.content.is_empty() {
        return false;
    }
    let record = message.extra.get(crate::ir::CARRIER_KEY);
    let claude_bag = message.harness_extra(crate::ir::Harness::Claude);
    let is_invoked_skills = record
        .and_then(|r| {
            if r.get("type").and_then(Value::as_str) != Some("attachment") {
                return None;
            }
            if r.get("isSidechain") == Some(&Value::Bool(true)) {
                return Some(false);
            }
            Some(r.pointer("/attachment/type").and_then(Value::as_str) == Some("invoked_skills"))
        })
        .or_else(|| {
            claude_bag.map(|b| {
                b.get("isSidechain") != Some(&Value::Bool(true))
                    && (b.get("attachment_type").and_then(Value::as_str) == Some("invoked_skills")
                        || b.get("attachment").and_then(|a| a.get("type")).and_then(Value::as_str)
                            == Some("invoked_skills"))
            })
        })
        .unwrap_or(false);
    if !is_invoked_skills {
        return false;
    }
    let skills = record
        .and_then(|r| r.pointer("/attachment/skills"))
        .or_else(|| claude_bag.and_then(|b| b.get("attachment").and_then(|a| a.get("skills"))))
        .and_then(Value::as_array);
    let Some(skills) = skills else {
        return false;
    };
    let cwd = record
        .and_then(|r| r.get("cwd"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| effective_cwd.map(Path::to_path_buf));
    skills
        .iter()
        .any(|entry| !(policy == SkillsPolicy::DropStale && current(entry, config, cwd.as_deref())))
}

/// A main-thread `attachment` record of type `invoked_skills`.
fn restore_record(source: &RangeSource, row: Range) -> Result<bool> {
    let mut cursor = source.cursor(row)?;
    cursor.whitespace()?;
    if cursor.position() == row.offset + row.len || source.accept(row, Validation::Value)?.is_none() {
        return Ok(false);
    }
    let is = |range: Option<Range>, want: &str| -> Result<bool> {
        Ok(match range {
            Some(r) if source.kind(r)? == Some(b'"') => source.materialize::<String>(r)? == want,
            _ => false,
        })
    };
    if !is(source.get(row, "type")?, "attachment")? {
        return Ok(false);
    }
    let Some(attachment) = source.get(row, "attachment")? else {
        return Ok(false);
    };
    if !is(source.get(attachment, "type")?, "invoked_skills")? {
        return Ok(false);
    }
    Ok(match source.get(row, "isSidechain")? {
        Some(r) => source.materialize::<Value>(r)? != Value::Bool(true),
        None => true,
    })
}

/// Whether a restored entry still matches its SKILL.md. An entry with no file to compare is kept.
fn current(entry: &Value, config: Option<&Path>, cwd: Option<&Path>) -> bool {
    let Some(paths) = skill_files(entry, config, cwd) else {
        return true;
    };
    for path in paths {
        let now = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            // Unreadable is not evidence that the skill is stale.
            Err(_) => return true,
        };
        let Some(content) = entry.get("content").and_then(Value::as_str) else {
            return true;
        };
        let now = body(&now);
        let (was, truncated) = stored_body(content);
        return if truncated {
            now.starts_with(was)
        } else {
            was == now
                || was
                    .strip_prefix(now)
                    .is_some_and(|rest| rest.starts_with('\n') && rest.trim_start().starts_with("ARGUMENTS:"))
        };
    }
    // Only NotFound at every candidate root proves the file is gone.
    false
}

/// Lookup precedence: skill directory, then legacy command, walking project ancestors from cwd.
/// Unknown origins or unsafe names cannot be judged. Nested command names use `a:b` → `a/b.md`.
fn skill_files(entry: &Value, config: Option<&Path>, cwd: Option<&Path>) -> Option<Vec<PathBuf>> {
    let (source, name) = entry.get("path")?.as_str()?.split_once(':')?;
    let name = Some(name)
        .filter(|n| !n.is_empty())
        .or_else(|| entry.get("name")?.as_str())?;
    if name.contains(['/', '\\'])
        || name.starts_with('.')
        || name.split(':').any(|p| p.is_empty() || p == "." || p == "..")
    {
        return None;
    }
    let roots = match source {
        "userSettings" => vec![config?.to_path_buf()],
        "projectSettings" => cwd?.ancestors().map(|d| d.join(".claude")).collect(),
        _ => return None,
    };
    Some(
        roots
            .into_iter()
            .flat_map(|root| {
                [
                    root.join("skills").join(name).join("SKILL.md"),
                    root.join("commands").join(format!("{}.md", name.replace(':', "/"))),
                ]
            })
            .collect(),
    )
}

/// A SKILL.md without its frontmatter.
fn body(text: &str) -> &str {
    let text = text
        .strip_prefix("---")
        .and_then(|rest| {
            let stop = rest.find("\n---")?;
            Some(rest[stop + 4..].split_once('\n').map_or("", |(_, after)| after))
        })
        .unwrap_or(text);
    text.trim()
}

/// Stored text without Claude Code's header and truncation marker: (body, truncated).
fn stored_body(content: &str) -> (&str, bool) {
    let content = match content.strip_prefix(BASE_DIR) {
        Some(rest) => rest.split_once("\n\n").map_or("", |(_, b)| b),
        None => content,
    };
    match content.strip_suffix(TRUNCATED) {
        Some(c) => (c.trim(), true),
        None => (content.trim(), false),
    }
}

#[cfg(test)]
mod tests;
