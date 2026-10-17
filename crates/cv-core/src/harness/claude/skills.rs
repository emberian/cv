//! What a derived Claude session restores of the skills its source invoked (`--skills-policy`).
//!
//! Claude Code (traced in 2.1.302) records each skill a session invokes with its text as it was
//! rendered then. Every compaction writes all of them back as one `invoked_skills` attachment, and
//! `--resume` rebuilds the store from the `invoked_skills` records after the last compact boundary.
//! Nothing else forgets an entry, so a skill invoked once is restored at every later compaction
//! with its old text, even after its SKILL.md changed or the skill was removed.
//!
//! A prune, rewind or port can drop those entries from the session it writes: `drop-all` drops
//! every one, `drop-stale` the ones whose SKILL.md is gone or no longer matches the stored text.
//! A rewritten record loses its stored `rendered*` fields, so Claude Code renders it again from
//! the kept list (an empty list renders nothing). A dropped skill stays invocable: the model gets
//! its current text when it uses it again. Only the new session is rewritten, never the source.

use super::replay::{copy, rows};
use crate::harness::store::{
    bytes::{ByteSource, ByteStage},
    document::range::{Range, RangeSource},
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
    DropAll,
    /// Entries whose SKILL.md is gone or differs from the stored text. Bundled and plugin skills
    /// have no file to compare and are kept.
    DropStale,
}

impl SkillsPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillsPolicy::Keep => "keep",
            SkillsPolicy::DropAll => "drop-all",
            SkillsPolicy::DropStale => "drop-stale",
        }
    }
}

impl std::str::FromStr for SkillsPolicy {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "keep" => SkillsPolicy::Keep,
            "drop-all" => SkillsPolicy::DropAll,
            "drop-stale" => SkillsPolicy::DropStale,
            _ => anyhow::bail!("skills policy {s:?}: expected keep, drop-all or drop-stale"),
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
    let projects = session.parent()?.parent()?;
    (projects.file_name()? == "projects").then(|| projects.parent().map(Path::to_path_buf))?
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
    let source = RangeSource::private(bytes.clone())?;
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

/// Apply the policy to a session file cv just wrote (a port's output), replacing it.
/// `config` is the Claude config dir whose user skills the session is judged against.
pub fn rewrite_file(path: &Path, policy: SkillsPolicy, config: Option<&Path>) -> Result<SkillsReport> {
    let mut stage = ByteStage::new();
    std::io::copy(&mut std::fs::File::open(path)?, &mut stage)?;
    let (output, report) = apply(stage.finish()?, policy, config)?;
    if report.records > 0 {
        crate::publish::publish_reader(path, "skills", output.reader()?, crate::publish::Claim::Replace)?;
    }
    Ok(report)
}

/// A main-thread `attachment` record of type `invoked_skills`.
fn restore_record(source: &RangeSource, row: Range) -> Result<bool> {
    let mut cursor = source.cursor(row)?;
    cursor.whitespace()?;
    if cursor.position() == row.offset + row.len || !source.validate(row)? {
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
    let Some(path) = skill_file(entry, config, cwd) else {
        return true;
    };
    let now = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => return e.kind() != std::io::ErrorKind::NotFound,
    };
    let now = body(&now);
    let (was, truncated) = stored_body(entry.get("content").and_then(Value::as_str).unwrap_or_default());
    if truncated {
        return now.starts_with(was);
    }
    // A stored text may carry the invocation's ARGUMENTS after the body.
    was == now || was.strip_prefix(now).is_some_and(|rest| rest.starts_with('\n'))
}

/// The SKILL.md a user (`userSettings:`) or project (`projectSettings:`) skill is read from.
fn skill_file(entry: &Value, config: Option<&Path>, cwd: Option<&Path>) -> Option<PathBuf> {
    let (source, name) = entry.get("path")?.as_str()?.split_once(':')?;
    let name = Some(name)
        .filter(|n| !n.is_empty())
        .or_else(|| entry.get("name")?.as_str())?;
    if name.contains(['/', '\\']) || name.starts_with('.') {
        return None;
    }
    let rel = Path::new("skills").join(name).join("SKILL.md");
    match source {
        "userSettings" => Some(config?.join(rel)),
        "projectSettings" => {
            let cwd = cwd?;
            let found = cwd
                .ancestors()
                .map(|d| d.join(".claude").join(&rel))
                .find(|p| p.exists());
            Some(found.unwrap_or_else(|| cwd.join(".claude").join(&rel)))
        }
        _ => None,
    }
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
