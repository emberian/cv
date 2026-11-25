//! `.cvbundle`: one session's portable closure in one file, readable after its source is gone.
//!
//! A bundle is a sealed seekable-zstd pack ([`crate::packs`]) behind a fixed header:
//!
//! ```text
//! b"CVBUNDLE" · u32 LE format version · u64 LE index length · the pack's index JSON · the .pack.zst bytes
//! ```
//!
//! The pack's records, by anchor: `bundle/manifest.json` ([`Manifest`]: what was captured and
//! what was not), `bundle/session.opensession.json` (the session as an OpenSession document),
//! `bundle/sidecar.flat.jsonl` (the prune/distill sidecar beside the source, when it has one),
//! and `files/<n>` (local files the session references: persisted tool outputs, media), each
//! under a size cap. A reference that is missing or over the cap is listed in
//! [`Manifest::uncaptured`] instead of silently dropped. Every record is BLAKE3-verified on read.

use crate::ir::{Block, Session};
use crate::packs::{PackBuilder, PackManifest, SealedPack};
use crate::publish::{Claim, StagedDir};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const MAGIC: &[u8; 8] = b"CVBUNDLE";
pub const FORMAT_VERSION: u32 = 1;
pub const EXTENSION: &str = "cvbundle";
const MANIFEST: &str = "bundle/manifest.json";
const SESSION: &str = "bundle/session.opensession.json";
const SIDECAR: &str = "bundle/sidecar.flat.jsonl";
/// One referenced file is captured up to this size.
pub const FILE_MAX_BYTES: u64 = 8 << 20;
/// All referenced files together are captured up to this size.
pub const TOTAL_MAX_BYTES: u64 = 64 << 20;

/// What a bundle holds and what it could not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub session_id: String,
    /// The harness the session was read from.
    pub harness: String,
    /// Where it was read from (informational: a bundle never reads it again).
    pub source_path: String,
    pub messages: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidecar: Option<Captured>,
    pub files: Vec<Captured>,
    pub uncaptured: Vec<UncapturedDep>,
    pub file_max_bytes: u64,
    pub total_max_bytes: u64,
}

/// The repository the session worked in, as the session recorded it and, when its working
/// directory still exists, as `git` reports it at bundle time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// `recorded` (from the session) or `observed` (from `git` at bundle time).
    pub from: String,
}

/// One captured file: the path the session names and the record that holds its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Captured {
    pub path: String,
    pub anchor: String,
    pub bytes: u64,
    pub blake3: String,
}

/// A path the session depends on that the bundle does not carry, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UncapturedDep {
    pub path: String,
    /// What referenced it: `persisted_output`, `image`, `file`, `working_tree`.
    pub kind: String,
    pub why: String,
}

/// What [`write`] bundles. `session` must be materialized.
pub struct Input<'a> {
    pub session: &'a Session,
    pub source_path: &'a Path,
    pub sidecar: Option<PathBuf>,
    pub git: Option<GitState>,
}

/// Is `path` a bundle (by extension, then by its magic)?
pub fn is_bundle(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
        return false;
    }
    let mut head = [0u8; 8];
    std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head))
        .is_ok_and(|()| &head == MAGIC)
}

/// The local files a session references: persisted tool outputs, image and file blocks with a
/// filesystem path. Inline data URIs and URLs are not references.
fn references(session: &Session) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = Vec::new();
    let mut push = |p: &str, kind: &'static str| {
        let local = p.starts_with('/') || p.starts_with("~/");
        if local && !out.iter().any(|(q, _)| q == p) {
            out.push((p.to_owned(), kind));
        }
    };
    for block in session.messages.iter().flat_map(|m| &m.content) {
        match block {
            Block::ToolResult { details, .. } => {
                if let Some(p) = details
                    .as_ref()
                    .and_then(|d| d.pointer("/persistedOutput/path"))
                    .and_then(Value::as_str)
                {
                    push(p, "persisted_output");
                }
            }
            Block::Image { data_ref: Some(p), .. } => push(p, "image"),
            Block::File { path: Some(p), .. } => push(p, "file"),
            _ => {}
        }
    }
    out
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => crate::env::home_dir().map_or_else(|| PathBuf::from(p), |h| h.join(rest)),
        None => PathBuf::from(p),
    }
}

/// Write `input` as a bundle at `dest`, published whole under `claim`.
pub fn write(dest: &Path, input: Input<'_>, claim: Claim) -> Result<Manifest> {
    let session = input.session;
    let staged = StagedDir::beside(dest, "bundle")?;
    let pack_id = "bundle";
    let mut pack = PackBuilder::create(staged.path(), pack_id)?;
    let document = serde_json::to_vec(&crate::harness::opensession::document(session))?;
    pack.append_record(SESSION, &document)?;

    let mut manifest = Manifest {
        version: FORMAT_VERSION,
        session_id: session.id.clone(),
        harness: session.harness.as_str().to_owned(),
        source_path: input.source_path.display().to_string(),
        messages: session.messages.len(),
        git: input.git,
        sidecar: None,
        files: Vec::new(),
        uncaptured: Vec::new(),
        file_max_bytes: FILE_MAX_BYTES,
        total_max_bytes: TOTAL_MAX_BYTES,
    };
    if let Some(sidecar) = input.sidecar.filter(|p| p.is_file()) {
        let bytes = std::fs::read(&sidecar).with_context(|| format!("reading sidecar {}", sidecar.display()))?;
        let addr = pack.append_record(SIDECAR, &bytes)?;
        manifest.sidecar = Some(Captured {
            path: sidecar.display().to_string(),
            anchor: SIDECAR.to_owned(),
            bytes: bytes.len() as u64,
            blake3: addr.digest_hex(),
        });
    }
    let mut total = 0u64;
    for (path, kind) in references(session) {
        let local = expand(&path);
        let uncaptured = |why: String| UncapturedDep {
            path: path.clone(),
            kind: kind.to_owned(),
            why,
        };
        let size = match std::fs::metadata(&local) {
            Ok(m) if m.is_file() => m.len(),
            Ok(_) => {
                manifest.uncaptured.push(uncaptured("not a regular file".into()));
                continue;
            }
            Err(e) => {
                manifest.uncaptured.push(uncaptured(format!("missing: {e}")));
                continue;
            }
        };
        if size > FILE_MAX_BYTES || total + size > TOTAL_MAX_BYTES {
            manifest
                .uncaptured
                .push(uncaptured(format!("{size} bytes, over the size cap")));
            continue;
        }
        let bytes = std::fs::read(&local).with_context(|| format!("reading {}", local.display()))?;
        total += bytes.len() as u64;
        let anchor = format!("files/{}", manifest.files.len());
        let addr = pack.append_record(anchor.clone(), &bytes)?;
        manifest.files.push(Captured {
            path,
            anchor,
            bytes: bytes.len() as u64,
            blake3: addr.digest_hex(),
        });
    }
    if let Some(cwd) = &session.cwd {
        manifest.uncaptured.push(UncapturedDep {
            path: cwd.display().to_string(),
            kind: "working_tree".into(),
            why: "the working tree is not bundled; git names the commit it stood on".into(),
        });
    }
    pack.append_record(MANIFEST, &serde_json::to_vec_pretty(&manifest)?)?;
    pack.seal(Claim::New)?;

    let zst = std::fs::read(crate::packs::pack_zst_path(staged.path(), pack_id)?)?;
    let idx = std::fs::read(crate::packs::pack_idx_path(staged.path(), pack_id)?)?;
    let mut bytes = Vec::with_capacity(20 + idx.len() + zst.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(idx.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&idx);
    bytes.extend_from_slice(&zst);
    crate::publish::publish_reader(dest, "cvbundle", &bytes[..], claim)?;
    // Read back before reporting success: the published file opens and names its session.
    let back = Bundle::open(dest)?;
    anyhow::ensure!(
        back.manifest == manifest,
        "bundle {} reads back differently",
        dest.display()
    );
    Ok(manifest)
}

/// An open bundle: its manifest and its verified records.
pub struct Bundle {
    pack: SealedPack,
    pub manifest: Manifest,
}

impl Bundle {
    pub fn open(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading bundle {}", path.display()))?;
        anyhow::ensure!(
            bytes.len() >= 20 && &bytes[..8] == MAGIC,
            "{} is not a cv bundle",
            path.display()
        );
        let version = u32::from_le_bytes(bytes[8..12].try_into()?);
        anyhow::ensure!(
            version == FORMAT_VERSION,
            "bundle format {version}; this cv reads {FORMAT_VERSION}"
        );
        let idx_len = usize::try_from(u64::from_le_bytes(bytes[12..20].try_into()?))?;
        let idx_end = 20usize.checked_add(idx_len).context("bundle index length overflows")?;
        anyhow::ensure!(idx_end <= bytes.len(), "bundle index runs past the file");
        let index: PackManifest = PackManifest::from_json_slice(&bytes[20..idx_end])?;
        let pack = SealedPack::from_slices(bytes[idx_end..].to_vec(), &bytes[20..idx_end])?;
        anyhow::ensure!(pack.pack_id() == index.pack_id, "bundle index names another pack");
        let manifest = pack.read_anchor(MANIFEST)?.context("bundle has no manifest")?;
        let manifest: Manifest = serde_json::from_slice(&manifest).context("bundle manifest")?;
        Ok(Self { pack, manifest })
    }

    /// The session's OpenSession document bytes.
    pub fn document(&self) -> Result<Vec<u8>> {
        self.pack.read_anchor(SESSION)?.context("bundle has no session")
    }

    /// A captured file's bytes, by the path the session names.
    pub fn file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        match self.manifest.files.iter().find(|f| f.path == path) {
            Some(f) => self.pack.read_anchor(&f.anchor),
            None => Ok(None),
        }
    }

    /// A stashed original from the bundled sidecar, by sidecar id (as `cv cat` asks for it):
    /// the entry's `content` value. Two entries with one id are an error, as in a sidecar file.
    pub fn stashed(&self, id: &str) -> Result<Option<Value>> {
        let Some(bytes) = self.pack.read_anchor(SIDECAR)? else {
            return Ok(None);
        };
        let mut found = None;
        for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let entry: Value = serde_json::from_slice(line).context("bundled sidecar entry")?;
            if entry.get("id").and_then(Value::as_str) == Some(id) {
                anyhow::ensure!(found.is_none(), "bundled sidecar holds {id:?} twice");
                found = entry.get("content").cloned();
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Harness, Message, MessageKind, Origin, Role};
    use std::path::Path;

    fn result(id: &str, content: &str, details: Option<Value>) -> Block {
        Block::ToolResult {
            tool_use_id: id.into(),
            content: content.into(),
            is_error: false,
            tool_name: None,
            status: None,
            details,
            extensions: Default::default(),
        }
    }

    // #tooth A bundle outlives its source: the session, its sidecar and its persisted output read
    // back after the source directory is deleted, and a missing reference is listed, not dropped.
    #[test]
    fn bundle_survives_source_deletion_and_lists_uncaptured() -> Result<()> {
        let world = crate::test_world("bundle-closure");
        let src = world.base.join("src");
        std::fs::create_dir_all(&src)?;
        let persisted = src.join("tool-results/big.txt");
        std::fs::create_dir_all(persisted.parent().unwrap())?;
        std::fs::write(&persisted, "the whole build log")?;
        let sidecar = src.join("s.flat.jsonl");
        std::fs::write(
            &sidecar,
            crate::prune::sidecar_line("toolu_1", "Bash", &Value::Null, "the original output") + "\n",
        )?;
        let mut session = Session::new("s", Harness::Codex);
        session.cwd = Some(src.join("tree"));
        let mut m = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::for_role(Role::Tool));
        m.content = vec![
            result(
                "toolu_2",
                "<persisted-output>",
                Some(serde_json::json!({"persistedOutput": {"path": persisted.to_str().unwrap()}})),
            ),
            result(
                "toolu_3",
                "x",
                Some(serde_json::json!({"persistedOutput": {"path": "/nonexistent/cv-bundle-test.txt"}})),
            ),
        ];
        session.messages.push(m);
        let dest = world.base.join("out.cvbundle");
        let manifest = write(
            &dest,
            Input {
                session: &session,
                source_path: &src.join("s.jsonl"),
                sidecar: Some(sidecar),
                git: Some(GitState {
                    head: Some("4f2a9c1".into()),
                    branch: Some("main".into()),
                    remote: None,
                    from: "recorded".into(),
                }),
            },
            Claim::New,
        )?;
        assert_eq!(manifest.files.len(), 1);
        let missing: Vec<_> = manifest.uncaptured.iter().map(|u| u.kind.as_str()).collect();
        assert_eq!(missing, ["persisted_output", "working_tree"]);
        std::fs::remove_dir_all(&src)?;
        assert!(is_bundle(&dest));
        let bundle = Bundle::open(&dest)?;
        assert_eq!(bundle.manifest, manifest);
        let back = crate::harness::opensession::from_str(std::str::from_utf8(&bundle.document()?)?)?;
        assert_eq!(back.messages.len(), 1);
        assert_eq!(bundle.stashed("toolu_1")?, Some(Value::from("the original output")));
        assert_eq!(bundle.stashed("absent")?, None);
        assert_eq!(
            bundle.file(persisted.to_str().unwrap())?.as_deref(),
            Some(&b"the whole build log"[..])
        );
        // A taken name is refused, and a non-bundle is not read as one.
        assert!(write(
            &dest,
            Input {
                session: &session,
                source_path: Path::new("x"),
                sidecar: None,
                git: None
            },
            Claim::New
        )
        .is_err());
        let fake = world.base.join("fake.cvbundle");
        std::fs::write(&fake, "not a bundle")?;
        assert!(!is_bundle(&fake));
        Ok(())
    }
}
