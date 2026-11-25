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
//! under a size cap. A reference that is missing, outside session containment, symlinked, or over
//! the cap is listed in [`Manifest::uncaptured`] instead of silently dropped. Every record is
//! BLAKE3-verified on read.
//!
//! With [`Input::redact`], every text record is scrubbed before it is sealed: the document, each
//! sidecar entry, each captured UTF-8 file, git branch/remote, principal, and local paths.
//! Non-UTF-8 payloads are omitted unless they carry a verified image header; [`Manifest::redactions`]
//! counts what was scrubbed.

use crate::ir::{Block, Session};
use crate::packs::{PackBuilder, PackManifest, SealedPack};
use crate::publish::{Claim, Staged, StagedDir};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

pub const MAGIC: &[u8; 8] = b"CVBUNDLE";
pub const FORMAT_VERSION: u32 = 1;
pub const EXTENSION: &str = "cvbundle";
const MANIFEST: &str = "bundle/manifest.json";
const SESSION: &str = "bundle/session.opensession.json";
const SIDECAR: &str = "bundle/sidecar.flat.jsonl";
/// One referenced file (or sidecar) is captured up to this size.
pub const FILE_MAX_BYTES: u64 = 8 << 20;
/// All referenced files together are captured up to this size.
pub const TOTAL_MAX_BYTES: u64 = 64 << 20;
/// Maximum index JSON byte length accepted on `Bundle::open`.
const MAX_INDEX_BYTES: u64 = 16 << 20;

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
    /// Spans scrubbed, when the bundle was written redacted; absent when it carries the session
    /// as recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redactions: Option<usize>,
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
    /// What referenced it: `persisted_output`, `image`, `file`, `sidecar`, `working_tree`.
    pub kind: String,
    pub why: String,
}

/// What [`write`] bundles. `session` must be materialized.
pub struct Input<'a> {
    pub session: &'a Session,
    pub source_path: &'a Path,
    pub sidecar: Option<PathBuf>,
    pub git: Option<GitState>,
    /// Scrub secrets from every text record before sealing (`cv export --format bundle --redact`).
    pub redact: Option<&'a crate::redact::RedactOptions>,
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

fn no_symlinks_below(canon_root: &Path, target: &Path) -> bool {
    let Ok(rel) = target.strip_prefix(canon_root) else {
        return false;
    };
    let mut cur = canon_root.to_path_buf();
    for part in rel.components() {
        let Component::Normal(name) = part else {
            return false;
        };
        cur.push(name);
        let Ok(meta) = std::fs::symlink_metadata(&cur) else {
            return false;
        };
        if meta.file_type().is_symlink() {
            return false;
        }
    }
    true
}

fn is_contained_reference(
    local: &Path,
    source_path: &Path,
    workspace_cwd: Option<&Path>,
) -> std::result::Result<(), String> {
    let sym = std::fs::symlink_metadata(local).map_err(|e| format!("missing: {e}"))?;
    if sym.file_type().is_symlink() {
        return Err("symlink".into());
    }
    if !sym.file_type().is_file() {
        return Err("not a regular file".into());
    }
    let canon = std::fs::canonicalize(local).map_err(|e| format!("missing: {e}"))?;

    let src_dir = source_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if let Ok(canon_src) = std::fs::canonicalize(src_dir) {
        // Check that `local` itself wasn't reached via a symlink inside `src_dir`.
        if let Ok(rel_spelled) = local.strip_prefix(src_dir) {
            if !no_symlinks_below(&canon_src, &canon_src.join(rel_spelled)) {
                return Err("symlink".into());
            }
        }
        if let Ok(rel) = canon.strip_prefix(&canon_src) {
            let comps: Vec<_> = rel.components().collect();
            let in_tool_results = (comps.len() == 2 && comps[0].as_os_str() == "tool-results")
                || (comps.len() == 3 && comps[1].as_os_str() == "tool-results");
            if in_tool_results && no_symlinks_below(&canon_src, &canon) {
                return Ok(());
            }
        }
        if let Some(canon_projects) = canon_src
            .parent()
            .filter(|p| p.file_name().is_some_and(|n| n == "projects"))
        {
            if let Ok(rel) = canon.strip_prefix(canon_projects) {
                let comps: Vec<_> = rel.components().collect();
                if comps.len() == 4
                    && comps[2].as_os_str() == "tool-results"
                    && no_symlinks_below(canon_projects, &canon)
                {
                    return Ok(());
                }
            }
        }
    }

    if let Some(cwd) = workspace_cwd.filter(|d| d.is_dir()) {
        if let Ok(canon_cwd) = std::fs::canonicalize(cwd) {
            let is_root = canon_cwd.parent().is_none();
            let is_home =
                crate::env::home_dir().and_then(|h| h.canonicalize().ok()).as_deref() == Some(canon_cwd.as_path());
            if !is_root && !is_home {
                if let Ok(rel_spelled) = local.strip_prefix(cwd) {
                    if !no_symlinks_below(&canon_cwd, &canon_cwd.join(rel_spelled)) {
                        return Err("symlink".into());
                    }
                }
                if no_symlinks_below(&canon_cwd, &canon) {
                    return Ok(());
                }
            }
        }
    }

    Err("outside session containment".into())
}

fn read_bounded_regular(path: &Path, max_bytes: u64) -> Result<std::result::Result<Vec<u8>, String>> {
    let sym = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => return Ok(Err(format!("missing: {e}"))),
    };
    if sym.file_type().is_symlink() {
        return Ok(Err("symlink".into()));
    }
    if !sym.file_type().is_file() {
        return Ok(Err("not a regular file".into()));
    }
    if sym.len() > max_bytes {
        return Ok(Err(format!("{} bytes, over the size cap", sym.len())));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(f) => f,
        Err(e) => return Ok(Err(format!("missing: {e}"))),
    };
    let meta = file
        .metadata()
        .with_context(|| format!("statting {}", path.display()))?;
    if !meta.file_type().is_file() {
        return Ok(Err("not a regular file".into()));
    }
    if meta.len() > max_bytes {
        return Ok(Err(format!("{} bytes, over the size cap", meta.len())));
    }
    let cap = usize::try_from(meta.len().min(64 * 1024)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(cap);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() as u64 > max_bytes {
        return Ok(Err(format!("{} bytes, over the size cap", bytes.len())));
    }
    Ok(Ok(bytes))
}

fn is_verified_image_magic(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
}

fn rel_posix(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn scrub_bundle_path(
    raw: &str,
    source_dir: Option<&Path>,
    workspace_cwd: Option<&Path>,
    opts: &crate::redact::RedactOptions,
    stats: &mut crate::redact::RedactStats,
) -> String {
    let expanded = expand(raw);
    let relativized = if let Some(cwd) = workspace_cwd {
        if expanded == cwd {
            Some("$REPO".to_owned())
        } else {
            expanded.strip_prefix(cwd).ok().map(|r| {
                let rel = rel_posix(r);
                if rel.is_empty() {
                    "$REPO".to_owned()
                } else {
                    format!("$REPO/{rel}")
                }
            })
        }
    } else {
        None
    }
    .or_else(|| {
        let src_dir = source_dir?;
        expanded
            .strip_prefix(src_dir)
            .ok()
            .map(rel_posix)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                let parent = src_dir.parent()?;
                expanded
                    .strip_prefix(parent)
                    .ok()
                    .map(rel_posix)
                    .filter(|s| !s.is_empty())
            })
    })
    .unwrap_or_else(|| {
        if raw.starts_with('/') || raw.starts_with("~/") || raw.contains('\\') {
            Path::new(raw)
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("redacted-path")
                .to_owned()
        } else {
            raw.to_owned()
        }
    });
    crate::redact::scrub_cow(&relativized, opts, stats).into_owned()
}

fn scrub_session_bundle_paths(
    session: &mut Session,
    source_dir: Option<&Path>,
    workspace_cwd: Option<&Path>,
    opts: &crate::redact::RedactOptions,
    stats: &mut crate::redact::RedactStats,
) {
    session.principal = None;
    if let Some(cwd) = session.cwd.as_mut() {
        let s = scrub_bundle_path(&cwd.to_string_lossy(), source_dir, workspace_cwd, opts, stats);
        *cwd = PathBuf::from(s);
    }
    if let Some(src) = session.source_path.as_mut() {
        let s = scrub_bundle_path(&src.to_string_lossy(), source_dir, workspace_cwd, opts, stats);
        *src = PathBuf::from(s);
    }
    for block in session.messages.iter_mut().flat_map(|m| &mut m.content) {
        match block {
            Block::ToolResult {
                details: Some(details), ..
            } => {
                for ptr in ["/persistedOutput/path", "/persistedOutput/unresolved"] {
                    if let Some(val) = details.pointer_mut(ptr) {
                        if let Some(s) = val.as_str() {
                            if s.starts_with('/') || s.starts_with("~/") {
                                *val = Value::String(scrub_bundle_path(s, source_dir, workspace_cwd, opts, stats));
                            }
                        }
                    }
                }
            }
            Block::Image { data_ref: Some(p), .. } if p.starts_with('/') || p.starts_with("~/") => {
                *p = scrub_bundle_path(p, source_dir, workspace_cwd, opts, stats);
            }
            Block::File { path: Some(p), .. } if p.starts_with('/') || p.starts_with("~/") => {
                *p = scrub_bundle_path(p, source_dir, workspace_cwd, opts, stats);
            }
            _ => {}
        }
    }
}

/// Write `input` as a bundle at `dest`, published whole under `claim`.
pub fn write(dest: &Path, input: Input<'_>, claim: Claim) -> Result<Manifest> {
    let orig_cwd = input.session.cwd.clone();
    let source_dir = input.source_path.parent().filter(|p| !p.as_os_str().is_empty());
    let raw_refs = references(input.session);

    let mut stats = crate::redact::RedactStats::default();
    let redacted;
    let session = match input.redact {
        Some(opts) => {
            let (mut clean, st) = crate::redact::redact_with(input.session, opts)?;
            stats = st;
            scrub_session_bundle_paths(&mut clean, source_dir, orig_cwd.as_deref(), opts, &mut stats);
            redacted = clean;
            &redacted
        }
        None => input.session,
    };
    let mut git = input.git;
    if let (Some(opts), Some(g)) = (input.redact, git.as_mut()) {
        if let Some(branch) = g.branch.as_mut() {
            *branch = crate::redact::scrub_cow(branch, opts, &mut stats).into_owned();
        }
        if let Some(remote) = g.remote.as_mut() {
            *remote = crate::redact::scrub_cow(remote, opts, &mut stats).into_owned();
        }
    }

    let fmt_path = |raw: &str, stats: &mut crate::redact::RedactStats| -> String {
        match input.redact {
            Some(opts) => scrub_bundle_path(raw, source_dir, orig_cwd.as_deref(), opts, stats),
            None => raw.to_owned(),
        }
    };

    let staged = StagedDir::beside(dest, "bundle")?;
    let pack_id = "bundle";
    let mut pack = PackBuilder::create(staged.path(), pack_id)?;
    let document = serde_json::to_vec(&crate::harness::opensession::document(session))?;
    pack.append_record(SESSION, &document)?;

    let manifest_source_path = fmt_path(&input.source_path.display().to_string(), &mut stats);
    let mut manifest = Manifest {
        version: FORMAT_VERSION,
        session_id: session.id.clone(),
        harness: session.harness.as_str().to_owned(),
        source_path: manifest_source_path,
        messages: session.messages.len(),
        git,
        sidecar: None,
        files: Vec::new(),
        uncaptured: Vec::new(),
        file_max_bytes: FILE_MAX_BYTES,
        total_max_bytes: TOTAL_MAX_BYTES,
        redactions: None,
    };
    if let Some(sidecar) = input.sidecar {
        let sidecar_disp = fmt_path(&sidecar.display().to_string(), &mut stats);
        match read_bounded_regular(&sidecar, FILE_MAX_BYTES)? {
            Ok(mut bytes) => {
                if let Some(opts) = input.redact {
                    match scrub_sidecar(&bytes, opts, &mut stats) {
                        Ok(clean) => bytes = clean,
                        Err(why) => {
                            manifest.uncaptured.push(UncapturedDep {
                                path: sidecar_disp.clone(),
                                kind: "sidecar".into(),
                                why: format!("{why:#}"),
                            });
                            bytes = Vec::new();
                        }
                    }
                }
                if !manifest.uncaptured.iter().any(|u| u.kind == "sidecar") {
                    let addr = pack.append_record(SIDECAR, &bytes)?;
                    manifest.sidecar = Some(Captured {
                        path: sidecar_disp,
                        anchor: SIDECAR.to_owned(),
                        bytes: bytes.len() as u64,
                        blake3: addr.digest_hex(),
                    });
                }
            }
            Err(why) => {
                manifest.uncaptured.push(UncapturedDep {
                    path: sidecar_disp,
                    kind: "sidecar".into(),
                    why,
                });
            }
        }
    }

    let mut total = 0u64;
    for (raw_path, kind) in raw_refs {
        let local = expand(&raw_path);
        let disp_path = fmt_path(&raw_path, &mut stats);
        let uncaptured = |why: String| UncapturedDep {
            path: disp_path.clone(),
            kind: kind.to_owned(),
            why,
        };
        if let Err(why) = is_contained_reference(&local, input.source_path, orig_cwd.as_deref()) {
            manifest.uncaptured.push(uncaptured(why));
            continue;
        }
        let mut bytes = match read_bounded_regular(&local, FILE_MAX_BYTES)? {
            Ok(b) => b,
            Err(why) => {
                manifest.uncaptured.push(uncaptured(why));
                continue;
            }
        };
        if total.saturating_add(bytes.len() as u64) > TOTAL_MAX_BYTES {
            manifest
                .uncaptured
                .push(uncaptured(format!("{} bytes, over the size cap", bytes.len())));
            continue;
        }
        if let Some(opts) = input.redact {
            match std::str::from_utf8(&bytes) {
                Ok(text) => {
                    if !(kind == "image" && is_verified_image_magic(&bytes)) {
                        if let std::borrow::Cow::Owned(clean) = crate::redact::scrub_cow(text, opts, &mut stats) {
                            bytes = clean.into_bytes();
                        }
                    }
                }
                Err(_) => {
                    if !(kind == "image" && is_verified_image_magic(&bytes)) {
                        manifest
                            .uncaptured
                            .push(uncaptured("non-UTF-8 content omitted under --redact".into()));
                        continue;
                    }
                }
            }
        }
        total += bytes.len() as u64;
        let anchor = format!("files/{}", manifest.files.len());
        let addr = pack.append_record(anchor.clone(), &bytes)?;
        manifest.files.push(Captured {
            path: disp_path,
            anchor,
            bytes: bytes.len() as u64,
            blake3: addr.digest_hex(),
        });
    }
    if let Some(cwd) = &orig_cwd {
        let cwd_disp = fmt_path(&cwd.display().to_string(), &mut stats);
        manifest.uncaptured.push(UncapturedDep {
            path: cwd_disp,
            kind: "working_tree".into(),
            why: "the working tree is not bundled; git names the commit it stood on".into(),
        });
    }
    if input.redact.is_some() {
        manifest.redactions = Some(stats.total());
    }
    pack.append_record(MANIFEST, &serde_json::to_vec_pretty(&manifest)?)?;
    pack.seal(Claim::New)?;

    let idx = std::fs::read(crate::packs::pack_idx_path(staged.path(), pack_id)?)?;
    let mut zst_file = std::fs::File::open(crate::packs::pack_zst_path(staged.path(), pack_id)?)?;
    let mut out_staged = Staged::beside(dest, "cvbundle", 0o600)?;
    {
        let mut w = std::io::BufWriter::with_capacity(64 * 1024, out_staged.file());
        w.write_all(MAGIC)?;
        w.write_all(&FORMAT_VERSION.to_le_bytes())?;
        w.write_all(&(idx.len() as u64).to_le_bytes())?;
        w.write_all(&idx)?;
        std::io::copy(&mut zst_file, &mut w)?;
        w.flush()?;
    }
    out_staged.publish(dest, claim)?;

    // Read back before reporting success: the published file opens and names its session.
    let back = Bundle::open(dest)?;
    anyhow::ensure!(
        back.manifest == manifest,
        "bundle {} reads back differently",
        dest.display()
    );
    Ok(manifest)
}

/// A sidecar with each entry scrubbed as JSON (its `content` and `input` alike); a line that is
/// not JSON is scrubbed as text. Lines stay one per entry.
fn scrub_sidecar(
    bytes: &[u8],
    opts: &crate::redact::RedactOptions,
    stats: &mut crate::redact::RedactStats,
) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(bytes).context("sidecar is not UTF-8")?;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, nl) = line.strip_suffix('\n').map_or((line, ""), |b| (b, "\n"));
        match serde_json::from_str::<Value>(body) {
            Ok(mut entry) => {
                crate::redact::scrub_value(&mut entry, opts, stats);
                out.push_str(&serde_json::to_string(&entry)?);
            }
            Err(_) => out.push_str(&crate::redact::scrub_cow(body, opts, stats)),
        }
        out.push_str(nl);
    }
    Ok(out.into_bytes())
}

/// An open bundle: its manifest and its verified records.
pub struct Bundle {
    pack: SealedPack,
    pub manifest: Manifest,
}

impl Bundle {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = std::fs::File::open(path).with_context(|| format!("reading bundle {}", path.display()))?;
        let file_len = file
            .metadata()
            .with_context(|| format!("statting bundle {}", path.display()))?
            .len();
        anyhow::ensure!(file_len >= 20, "{} is not a cv bundle", path.display());
        let mut head = [0u8; 20];
        file.read_exact(&mut head)
            .with_context(|| format!("reading bundle header {}", path.display()))?;
        anyhow::ensure!(&head[..8] == MAGIC, "{} is not a cv bundle", path.display());
        let version = u32::from_le_bytes(head[8..12].try_into()?);
        anyhow::ensure!(
            version == FORMAT_VERSION,
            "bundle format {version}; this cv reads {FORMAT_VERSION}"
        );
        let idx_len = u64::from_le_bytes(head[12..20].try_into()?);
        anyhow::ensure!(idx_len <= MAX_INDEX_BYTES, "bundle index exceeds size limit");
        let idx_end = 20u64.checked_add(idx_len).context("bundle index length overflows")?;
        anyhow::ensure!(idx_end <= file_len, "bundle index runs past the file");
        let mut idx_bytes = vec![0u8; usize::try_from(idx_len)?];
        file.read_exact(&mut idx_bytes)
            .with_context(|| format!("reading bundle index {}", path.display()))?;
        let index: PackManifest = PackManifest::from_json_slice(&idx_bytes)?;
        let pack = SealedPack::open_file_slice(path, file, idx_end, file_len - idx_end, &idx_bytes)?;
        anyhow::ensure!(pack.pack_id() == index.pack_id, "bundle index names another pack");
        let manifest = pack.read_anchor(MANIFEST)?.context("bundle has no manifest")?;
        let manifest: Manifest = serde_json::from_slice(&manifest).context("bundle manifest")?;
        Ok(Self { pack, manifest })
    }

    /// Number of compressed frames decoded through this bundle handle so far.
    pub fn frames_decoded(&self) -> u64 {
        self.pack.frames_decoded()
    }

    /// The session's OpenSession document bytes.
    pub fn document(&self) -> Result<Vec<u8>> {
        self.pack.read_anchor(SESSION)?.context("bundle has no session")
    }

    /// A captured file's bytes, by the path the session names (or an unambiguous suffix match when
    /// paths were relativized under `--redact`).
    pub fn file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let exact = self.manifest.files.iter().find(|f| f.path == path);
        let matched = exact.or_else(|| {
            let candidates: Vec<_> = self
                .manifest
                .files
                .iter()
                .filter(|f| {
                    let rel = f.path.strip_prefix("$REPO/").unwrap_or(&f.path);
                    !rel.starts_with('/') && path.ends_with(&format!("/{rel}"))
                })
                .collect();
            (candidates.len() == 1).then(|| candidates[0])
        });
        match matched {
            Some(f) => self.pack.read_anchor(&f.anchor),
            None => Ok(None),
        }
    }

    /// A stashed original from the bundled sidecar matching `(id, slot)` (`"content"`, `"input"`,
    /// `"toolUseResult"`, or `"record"`). Two entries with the same `(id, slot)` are an error.
    pub fn stashed_slot(&self, id: &str, slot: &str) -> Result<Option<Value>> {
        let Some(bytes) = self.pack.read_anchor(SIDECAR)? else {
            return Ok(None);
        };
        let mut found = None;
        for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let entry: Value = serde_json::from_slice(line).context("bundled sidecar entry")?;
            let entry_id = entry.get("id").and_then(Value::as_str);
            let entry_slot = entry.get("slot").and_then(Value::as_str).unwrap_or("content");
            if entry_id == Some(id) && entry_slot == slot {
                anyhow::ensure!(found.is_none(), "bundled sidecar holds ({id:?}, {slot:?}) twice");
                found = entry.get("content").cloned();
            }
        }
        Ok(found)
    }

    /// A stashed result/payload from the bundled sidecar by sidecar id (as `cv cat` asks for it):
    /// prefers `slot == "content"`, falling back to non-`"input"` slots (such as `"toolUseResult"`)
    /// so an `id` that stashed both `"input"` and `"content"` resolves cleanly without ambiguity.
    pub fn stashed(&self, id: &str) -> Result<Option<Value>> {
        if let Some(v) = self.stashed_slot(id, "content")? {
            return Ok(Some(v));
        }
        let Some(bytes) = self.pack.read_anchor(SIDECAR)? else {
            return Ok(None);
        };
        let mut found = None;
        for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let entry: Value = serde_json::from_slice(line).context("bundled sidecar entry")?;
            let entry_id = entry.get("id").and_then(Value::as_str);
            let entry_slot = entry.get("slot").and_then(Value::as_str).unwrap_or("content");
            if entry_id == Some(id) && entry_slot != "input" {
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
    use crate::ir::{GitInfo as GitContext, Harness, Message, MessageKind, Origin, Principal, Role};
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
                redact: None,
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
                git: None,
                redact: None,
            },
            Claim::New
        )
        .is_err());
        let fake = world.base.join("fake.cvbundle");
        std::fs::write(&fake, "not a bundle")?;
        assert!(!is_bundle(&fake));
        Ok(())
    }

    // #tooth A redacted bundle carries no secret in any record: the document, the sidecar's
    // stashed original, a captured tool output, principal, git branch/remote, and local paths
    // are scrubbed before sealing, and non-UTF-8 non-image blobs are never sealed unredacted.
    #[test]
    fn redacted_bundle_scrubs_document_sidecar_files_and_remote() -> Result<()> {
        let world = crate::test_world("bundle-redact");
        let src = world.base.join("src");
        let tool_results = src.join("tool-results");
        std::fs::create_dir_all(&tool_results)?;
        let key = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let persisted = tool_results.join("out.txt");
        std::fs::write(&persisted, format!("export ANTHROPIC_API_KEY={key}\n"))?;
        let binary_leak = tool_results.join("raw.bin");
        let mut raw_bytes = vec![0xff, 0xfe, 0x00];
        raw_bytes.extend_from_slice(key.as_bytes());
        std::fs::write(&binary_leak, &raw_bytes)?;
        let png = tool_results.join("ok.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR")?;

        let sidecar = src.join("s.flat.jsonl");
        std::fs::write(
            &sidecar,
            crate::prune::sidecar_line("toolu_1", "Bash", &Value::Null, &format!("token {key}")) + "\n",
        )?;
        let mut session = Session::new("s", Harness::Codex);
        session.cwd = Some(world.base.join("secret-workspace"));
        std::fs::create_dir_all(session.cwd.as_ref().unwrap())?;
        session.principal = Some(Principal::String(format!("alice@example.com ({key})")));
        session.git = Some(GitContext {
            branch: Some(format!("feat-{key}")),
            commit: Some("deadbeef".into()),
            remote: Some(format!("https://x-access-token:{key}@example.com/o/r.git")),
            extensions: Default::default(),
        });
        let mut m = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::for_role(Role::Tool));
        m.content = vec![
            result(
                "toolu_2",
                &format!("printed {key}"),
                Some(serde_json::json!({"persistedOutput": {"path": persisted.to_str().unwrap()}})),
            ),
            result(
                "toolu_3",
                "<persisted-output>",
                Some(serde_json::json!({"persistedOutput": {"path": binary_leak.to_str().unwrap()}})),
            ),
            Block::Image {
                media_type: Some("image/png".into()),
                data_ref: Some(png.to_str().unwrap().into()),
                extensions: Default::default(),
            },
        ];
        session.messages.push(m);
        let dest = world.base.join("clean.cvbundle");
        let opts = crate::redact::RedactOptions::default();
        let manifest = write(
            &dest,
            Input {
                session: &session,
                source_path: &src.join("s.jsonl"),
                sidecar: Some(sidecar),
                git: Some(GitState {
                    branch: Some(format!("feat-{key}")),
                    remote: Some(format!("https://x-access-token:{key}@example.com/o/r.git")),
                    from: "recorded".into(),
                    ..GitState::default()
                }),
                redact: Some(&opts),
            },
            Claim::New,
        )?;
        assert!(manifest.redactions.is_some_and(|n| n >= 5), "{manifest:?}");
        let manifest_json = serde_json::to_string(&manifest)?;
        let base_str = world.base.to_string_lossy();
        assert!(!manifest_json.contains(key), "{manifest_json}");
        assert!(
            !manifest_json.contains(base_str.as_ref()),
            "manifest leaked local path: {manifest_json}"
        );
        // Binary persisted_output was omitted to uncaptured; verified PNG was captured.
        assert_eq!(manifest.files.len(), 2, "{manifest:?}");
        assert!(
            manifest
                .uncaptured
                .iter()
                .any(|u| u.kind == "persisted_output" && u.why.contains("non-UTF-8")),
            "{manifest:?}"
        );

        let bundle = Bundle::open(&dest)?;
        let doc = String::from_utf8(bundle.document()?)?;
        assert!(!doc.contains(key) && doc.contains("printed"), "{doc}");
        assert!(!doc.contains("alice@example.com"), "principal leaked: {doc}");
        assert!(!doc.contains(base_str.as_ref()), "doc leaked local path: {doc}");
        let stashed = bundle.stashed("toolu_1")?.unwrap();
        assert!(!stashed.to_string().contains(key) && stashed.to_string().contains("token"));
        let file = String::from_utf8(bundle.file(persisted.to_str().unwrap())?.unwrap())?;
        assert!(
            !file.contains(key) && file.starts_with("export ANTHROPIC_API_KEY="),
            "{file}"
        );
        assert!(bundle.file("tool-results/out.txt")?.is_some());
        assert!(bundle.file(binary_leak.to_str().unwrap())?.is_none());
        let git = bundle.manifest.git.unwrap();
        assert!(!git.remote.unwrap().contains(key));
        assert!(!git.branch.unwrap().contains(key));
        Ok(())
    }

    // #tooth Files outside session containment and symlinks inside `tool-results/` or `cwd` are
    // refused into `uncaptured` without reading their targets.
    #[cfg(unix)]
    #[test]
    fn bundle_blocks_path_exfiltration_and_symlinks() -> Result<()> {
        let world = crate::test_world("bundle-containment");
        let src = world.base.join("src");
        let tool_results = src.join("tool-results");
        let cwd = world.base.join("workspace");
        let outside_dir = world.base.join("outside");
        std::fs::create_dir_all(&tool_results)?;
        std::fs::create_dir_all(&cwd)?;
        std::fs::create_dir_all(&outside_dir)?;

        let secret_outside = outside_dir.join("secret.txt");
        std::fs::write(&secret_outside, "TOP_SECRET_OUTSIDE")?;
        let symlink_in_tr = tool_results.join("link.txt");
        std::os::unix::fs::symlink(&secret_outside, &symlink_in_tr)?;
        let symlink_in_cwd = cwd.join("cwd_link.txt");
        std::os::unix::fs::symlink(&secret_outside, &symlink_in_cwd)?;
        let valid_in_cwd = cwd.join("inside.txt");
        std::fs::write(&valid_in_cwd, "ALLOWED_WORKSPACE_FILE")?;

        let mut session = Session::new("s", Harness::Claude);
        session.cwd = Some(cwd);
        let mut m = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::for_role(Role::Tool));
        m.content = vec![
            result(
                "t_outside",
                "<persisted-output>",
                Some(serde_json::json!({"persistedOutput": {"path": secret_outside.to_str().unwrap()}})),
            ),
            result(
                "t_tr_sym",
                "<persisted-output>",
                Some(serde_json::json!({"persistedOutput": {"path": symlink_in_tr.to_str().unwrap()}})),
            ),
            Block::File {
                mime: Some("text/plain".into()),
                path: Some(symlink_in_cwd.to_str().unwrap().into()),
                source: None,
                extensions: Default::default(),
            },
            Block::File {
                mime: Some("text/plain".into()),
                path: Some(valid_in_cwd.to_str().unwrap().into()),
                source: None,
                extensions: Default::default(),
            },
        ];
        session.messages.push(m);

        let dest = world.base.join("contained.cvbundle");
        let manifest = write(
            &dest,
            Input {
                session: &session,
                source_path: &src.join("s.jsonl"),
                sidecar: None,
                git: None,
                redact: None,
            },
            Claim::New,
        )?;
        assert_eq!(manifest.files.len(), 1, "{manifest:?}");
        assert_eq!(manifest.files[0].path, valid_in_cwd.to_str().unwrap());
        assert!(
            manifest
                .uncaptured
                .iter()
                .any(|u| u.path == secret_outside.to_str().unwrap() && u.why.contains("containment")),
            "{manifest:?}"
        );
        assert!(
            manifest
                .uncaptured
                .iter()
                .any(|u| u.path == symlink_in_tr.to_str().unwrap() && u.why == "symlink"),
            "{manifest:?}"
        );
        assert!(
            manifest
                .uncaptured
                .iter()
                .any(|u| u.path == symlink_in_cwd.to_str().unwrap() && u.why == "symlink"),
            "{manifest:?}"
        );
        Ok(())
    }

    // #tooth Opening a multi-frame bundle reads only the manifest frame (seeking payload frames on
    // demand), and a sidecar entry sharing `id` across `"input"` and `"content"` slots disambiguates.
    #[test]
    fn bundle_opens_lazily_and_disambiguates_sidecar_slots() -> Result<()> {
        let world = crate::test_world("bundle-lazy-slots");
        let src = world.base.join("src");
        let tool_results = src.join("tool-results");
        std::fs::create_dir_all(&tool_results)?;
        // Pseudo-random 3 MiB file spans >1 2 MiB seekable frame; `Bundle::open` must not decode it!
        let mut big = vec![0u8; 3 * 1024 * 1024];
        let mut state = 0x1234_5678_9abc_def0u64;
        for chunk in big.chunks_exact_mut(8) {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            chunk.copy_from_slice(&state.to_le_bytes());
        }
        let big_txt = tool_results.join("large.bin");
        std::fs::write(&big_txt, &big)?;

        let sidecar = src.join("s.flat.jsonl");
        let both_slots = format!(
            "{}\n{}\n",
            serde_json::json!({
                "id": "toolu_dual",
                "slot": "input",
                "name": "Write",
                "input": null,
                "content": "{\"file_path\":\"/w/a.rs\",\"content\":\"fn a() {}\"}",
                "size": 42,
                "line_count": 1,
                "kind": "text"
            }),
            serde_json::json!({
                "id": "toolu_dual",
                "slot": "content",
                "name": "Write",
                "input": null,
                "content": "wrote /w/a.rs",
                "size": 13,
                "line_count": 1,
                "kind": "text"
            }),
        );
        std::fs::write(&sidecar, both_slots)?;

        let mut session = Session::new("s", Harness::Claude);
        let mut m = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::for_role(Role::Tool));
        m.content = vec![result(
            "toolu_big",
            "<persisted-output>",
            Some(serde_json::json!({"persistedOutput": {"path": big_txt.to_str().unwrap()}})),
        )];
        session.messages.push(m);

        let dest = world.base.join("lazy.cvbundle");
        write(
            &dest,
            Input {
                session: &session,
                source_path: &src.join("s.jsonl"),
                sidecar: Some(sidecar),
                git: None,
                redact: None,
            },
            Claim::New,
        )?;

        let bundle = Bundle::open(&dest)?;
        // Only the trailing frame holding `bundle/manifest.json` was decoded at open time.
        assert_eq!(bundle.frames_decoded(), 1);
        assert_eq!(bundle.stashed("toolu_dual")?, Some(Value::from("wrote /w/a.rs")));
        assert_eq!(
            bundle.stashed_slot("toolu_dual", "input")?,
            Some(Value::from("{\"file_path\":\"/w/a.rs\",\"content\":\"fn a() {}\"}"))
        );
        assert_eq!(bundle.file(big_txt.to_str().unwrap())?.as_deref(), Some(&big[..]));
        assert!(bundle.frames_decoded() >= 2);
        Ok(())
    }
}
