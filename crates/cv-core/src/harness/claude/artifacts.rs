//! Files addressed by tool call ID in a continuation. A captured spill has a
//! transcript-local name, independent of the absolute path in the original receipt.
use super::{carry::check_regular_dest_file, record, sidecar::session_dir};
use crate::json::JsonView;
use crate::publish::{Claim, Set, Staged};
use anyhow::{ensure, Context, Result};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// A shared database has many sessions. Give each one its own sidecar namespace.
pub fn anchor(path: &Path, id: &str) -> PathBuf {
    if matches!(
        path.extension().and_then(|v| v.to_str()),
        Some("db" | "sqlite" | "sqlite3")
    ) {
        path.with_file_name(format!(
            "{}.{}.jsonl",
            path.file_name().unwrap().to_string_lossy(),
            crate::digest::sha256_hex(id.as_bytes())
        ))
    } else {
        path.to_path_buf()
    }
}

fn spill_path(transcript: &Path, id: &str) -> Result<PathBuf> {
    Ok(session_dir(transcript)
        .context("transcript has no stem")?
        .join("tool-results")
        .join(format!("{}.txt", crate::digest::sha256_hex(id.as_bytes()))))
}

// Check each component below the trusted store root, including intermediate symlinks.
fn directories(root: &Path, dir: &Path, create: bool) -> Result<bool> {
    let mut current = root.to_path_buf();
    for part in dir.strip_prefix(root)?.components() {
        ensure!(
            matches!(part, std::path::Component::Normal(_)),
            "invalid artifact directory"
        );
        current.push(part);
        if create {
            match fs::create_dir(&current) {
                Ok(()) => continue,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        match fs::symlink_metadata(&current) {
            Ok(m) => ensure!(m.is_dir(), "{} is not a regular directory", current.display()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}

pub fn captured_output(transcript: &Path, id: &str) -> Result<Option<PathBuf>> {
    let path = spill_path(transcript, id)?;
    Ok((directories(
        transcript.parent().context("missing transcript directory")?,
        path.parent().unwrap(),
        false,
    )? && check_regular_dest_file(&path)?)
    .then_some(path))
}

#[derive(Default)]
pub struct Artifacts {
    flat: Vec<PathBuf>,
    spills: BTreeMap<String, PathBuf>,
}

impl Artifacts {
    pub fn is_empty(&self) -> bool {
        self.flat.is_empty() && self.spills.is_empty()
    }

    /// Verify every referenced source file and merged sidecar before writing target output.
    pub fn validate(&self) -> Result<()> {
        for source in self.spills.values() {
            digest(source)?;
        }
        let mut entries = BTreeMap::new();
        for file in &self.flat {
            let check_conflicts = self.flat.len() > 1;
            read_regular(file, |source| {
                for line in BufReader::new(source).lines() {
                    let line = line?;
                    if line.is_empty() {
                        continue;
                    }
                    let row: serde_json::Value = serde_json::from_str(&line)?;
                    let id = row["id"].as_str().context("flat entry has no ID")?;
                    if check_conflicts {
                        let hash = blake3::hash(&serde_json::to_vec(&row)?);
                        if let Some(previous) = entries.insert(id.to_owned(), hash) {
                            ensure!(previous == hash, "conflicting sidecar entry {id:?}");
                        }
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn collect_flat(&mut self, transcript: &Path) -> Result<Option<PathBuf>> {
        let flat = transcript.with_extension("flat.jsonl");
        if !check_regular_dest_file(&flat)? {
            return Ok(None);
        }
        if !self.flat.contains(&flat) {
            self.flat.push(flat.clone());
        }
        Ok(Some(flat))
    }

    /// Collect sidecar and spilled tool-result artifacts from either a native Claude transcript or
    /// a session previously ported through another format.
    pub fn collect_source(&mut self, r: &crate::SessionRef, session: &crate::Session) -> Result<()> {
        if r.harness == crate::Harness::Claude {
            self.collect(&r.path)
        } else {
            self.collect_ported(&anchor(&r.path, &session.id), session)
        }
    }

    /// Recover artifacts previously carried through a non-Claude format. Only open
    /// files under this transcript's anchor, addressed by IDs actually present in it.
    pub fn collect_ported(&mut self, transcript: &Path, session: &crate::Session) -> Result<()> {
        self.collect_flat(transcript)?;
        for block in session.messages.iter().flat_map(|m| &m.content) {
            if let crate::Block::ToolResult { tool_use_id, .. } = block {
                if let Some(path) = captured_output(transcript, tool_use_id)? {
                    self.spill(tool_use_id, path)?;
                }
            }
        }
        Ok(())
    }

    pub fn collect(&mut self, transcript: &Path) -> Result<()> {
        let flat = self.collect_flat(transcript)?;
        crate::harness::store::jsonl::for_each_record(transcript, |row| {
            let Some(message) = row.get("message")? else {
                return Ok(());
            };
            let Some(body) = message.get("content")? else {
                return Ok(());
            };
            let Some(blocks) = body.array()? else { return Ok(()) };
            for block in blocks {
                let block = block?;
                if block.get("type")?.map(|v| v.string()).transpose()?.flatten().as_deref() != Some("tool_result") {
                    continue;
                }
                let Some(id) = block.get("tool_use_id")?.map(|v| v.string()).transpose()?.flatten() else {
                    continue;
                };
                if let Some(path) = captured_output(transcript, &id)? {
                    self.spill(&id, path)?;
                } else if let Some(body) = block.get("content")? {
                    self.collect_body(transcript, &id, body)?;
                }
            }
            Ok(())
        })?;
        // A large native spill receipt can itself have been elided by prune.
        if let Some(flat) = flat {
            crate::harness::store::jsonl::for_each_record(&flat, |row| {
                if row.get("slot")?.map(|v| v.string()).transpose()?.flatten().as_deref() == Some("content") {
                    if let (Some(id), Some(body)) = (
                        row.get("id")?.map(|v| v.string()).transpose()?.flatten(),
                        row.get("content")?,
                    ) {
                        if !self.spills.contains_key(&id) {
                            self.collect_body(transcript, &id, body)?;
                        }
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn collect_body(&mut self, transcript: &Path, id: &str, body: JsonView<'_>) -> Result<()> {
        let mut capture = |text: JsonView<'_>| -> Result<()> {
            const PREFIX: &str = "<persisted-output>";
            if !text.is_string()? || record::string_head(text, PREFIX.len())? != PREFIX {
                return Ok(());
            }
            let Some((claim, _)) = super::persisted_output(&record::string_head(text, 8192)?) else {
                return Ok(());
            };
            let Some(path) = record::spilled_output(transcript, &claim) else {
                return Ok(());
            };
            let root = crate::rewind::project_dir(transcript);
            let scope = if path.starts_with(&root) {
                root.as_path()
            } else {
                root.parent().context("missing project root")?
            };
            if directories(scope, path.parent().unwrap(), false)? && check_regular_dest_file(&path)? {
                self.spill(id, path)?;
            }
            Ok(())
        };
        if let Some(items) = body.array()? {
            for item in items {
                if let Some(text) = item?.get("text")? {
                    capture(text)?;
                }
            }
        } else {
            capture(body)?;
        }
        Ok(())
    }

    fn spill(&mut self, id: &str, path: PathBuf) -> Result<()> {
        if let Some(previous) = self.spills.get(id) {
            ensure!(
                previous == &path || digest(previous)? == digest(&path)?,
                "conflicting spilled output for {id:?}"
            );
        } else {
            self.spills.insert(id.to_owned(), path);
        }
        Ok(())
    }

    pub fn stage(&self, dest: &Path, set: &mut Set, claim: Claim) -> Result<()> {
        for (id, source) in &self.spills {
            let path = spill_path(dest, id)?;
            directories(
                dest.parent().context("missing destination directory")?,
                path.parent().unwrap(),
                true,
            )?;
            if check_regular_dest_file(&path)? && digest(&path)? == digest(source)? {
                continue;
            }
            let mut staged = Staged::beside(&path, "tool-output", 0o600)?;
            read_regular(source, |file| Ok(io::copy(file, &mut staged)?))?;
            set.add(staged, path, claim);
        }
        let path = dest.with_extension("flat.jsonl");
        if self.flat.is_empty() {
            if claim == Claim::Replace && check_regular_dest_file(&path)? {
                // Replacing a transcript also retires its old retrieval IDs.
                set.add(Staged::beside(&path, "flat", 0o600)?, path, claim);
            }
            return Ok(());
        }
        let existing = check_regular_dest_file(&path)?;
        if existing && self.flat.len() == 1 && digest(&path)? == digest(&self.flat[0])? {
            return Ok(());
        }
        ensure!(
            !existing || claim == Claim::Replace,
            "conflicting existing sidecar {}",
            path.display()
        );
        let mut staged = Staged::beside(&path, "flat", 0o600)?;
        if self.flat.len() == 1 {
            read_regular(&self.flat[0], |file| Ok(io::copy(file, &mut staged)?))?;
        } else {
            let mut entries = BTreeMap::new();
            for file in &self.flat {
                read_regular(file, |source| {
                    for line in BufReader::new(source).lines() {
                        let line = line?;
                        if line.is_empty() {
                            continue;
                        }
                        let row: serde_json::Value = serde_json::from_str(&line)?;
                        let id = row["id"].as_str().context("flat entry has no ID")?;
                        let hash = blake3::hash(&serde_json::to_vec(&row)?);
                        if let Some(previous) = entries.insert(id.to_owned(), hash) {
                            ensure!(previous == hash, "conflicting sidecar entry {id:?}");
                        } else {
                            writeln!(staged, "{line}")?;
                        }
                    }
                    Ok(())
                })?;
            }
        }
        set.add(staged, path, claim);
        Ok(())
    }
}

fn read_regular<T>(path: &Path, read: impl FnOnce(&mut File) -> Result<T>) -> Result<T> {
    use crate::source::FileId;
    let meta = fs::symlink_metadata(path)?;
    ensure!(meta.is_file(), "{} is not a regular file", path.display());
    let identity = FileId::of(&meta).context("missing artifact identity")?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    ensure!(
        FileId::of(&file.metadata()?) == Some(identity),
        "artifact changed before opening {}",
        path.display()
    );
    let value = read(&mut file)?;
    ensure!(
        FileId::of(&fs::symlink_metadata(path)?) == Some(identity),
        "artifact changed while reading {}",
        path.display()
    );
    Ok(value)
}

fn digest(path: &Path) -> Result<blake3::Hash> {
    read_regular(path, |file| {
        let mut hasher = blake3::Hasher::new();
        io::copy(file, &mut hasher)?;
        Ok(hasher.finalize())
    })
}
