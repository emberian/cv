//! Staged publication, the one way cv puts a finished file under a name readers may already watch.
//!
//! A [`Staged`] file is written under a private temporary name in the destination's directory,
//! fsynced, and then claimed by name in one step, after which the directory is fsynced. A reader
//! of the destination, including a live mmap, sees no file, the whole previous file, or the whole
//! new one; never a truncated or partial one.
//!
//! - [`Claim::New`] refuses any existing name, a dangling symlink included, with [`Exists`]. It
//!   hard-links the temporary to the destination. Where the filesystem has no hard links (FAT,
//!   exFAT, some network mounts) it renames without replacing instead: `renamex_np(RENAME_EXCL)`
//!   on macOS, `renameat2(RENAME_NOREPLACE)` on Linux. Without either exclusive operation it fails
//!   closed: checking a name before an ordinary rename could overwrite a concurrent writer.
//! - [`Claim::Replace`] renames over the destination. The name moves to the new inode; a reader
//!   holding the old one keeps the old bytes, and a symlink at the destination is replaced, not
//!   followed.
//!
//! A [`Set`] publishes several files as one: every name takes its new file, or every name keeps
//! what it held, and a failure to undo is reported.
use anyhow::{Context, Result};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// How [`Staged::publish`] claims its destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    New,
    Replace,
}

/// A [`Claim::New`] destination was already taken. Callers that refuse an existing output before
/// writing return it too, so every refusal reads the same and the CLI exits 2 on it.
#[derive(Debug, thiserror::Error)]
#[error("{} already exists, refusing to overwrite it{hint}", path.display())]
pub struct Exists {
    pub path: PathBuf,
    /// What the caller can do instead, `" (pick another --to)"`, or empty.
    pub hint: &'static str,
}

/// Refuse `path` if anything, a dangling symlink included, holds the name.
pub fn refuse_existing(path: &Path, hint: &'static str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(Exists {
            path: path.to_owned(),
            hint,
        }
        .into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("checking {}", path.display())),
    }
}

/// Validate a caller-supplied session identifier before using it as a filename or directory
/// component in an output store. Rejects empty strings, path separators, NULs, `.`, and `..`.
pub fn validate_session_id(id: &str) -> anyhow::Result<()> {
    let mut components = std::path::Path::new(id).components();
    anyhow::ensure!(
        !id.is_empty()
            && !id.contains(['/', '\\', '\0'])
            && matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none(),
        "new session id must be a nonempty filename component, not a path: {id:?}"
    );
    Ok(())
}

/// Raw transform output ids also become filenames/resource directory names. Keep them within the
/// selected output directory; this does not restrict ids stored in arbitrary IR or restamped text.
pub(crate) fn validate_output_id(id: &str) -> anyhow::Result<()> {
    validate_session_id(id)
}

/// An exclusively created temporary that only this operation owns; dropped unpublished, it is
/// removed. Writes go straight to the file (wrap it in a `BufWriter` for small writes).
pub struct Staged {
    path: PathBuf,
    file: Option<File>,
}

impl Staged {
    /// A new temporary `.cv-<tag>-<uuid>.tmp` in `dir`, created with `mode` (before the umask) on
    /// unix. Publication keeps that mode.
    pub fn create(dir: &Path, tag: &str, mode: u32) -> Result<Self> {
        let path = dir.join(format!(".cv-{tag}-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let file = options
            .open(&path)
            .with_context(|| format!("creating a temporary in {}", dir.display()))?;
        Ok(Self { path, file: Some(file) })
    }

    /// [`Self::create`] in `destination`'s directory, so publication never crosses a filesystem.
    pub fn beside(destination: &Path, tag: &str, mode: u32) -> Result<Self> {
        Self::create(parent_dir(destination), tag, mode)
    }

    /// The temporary's own name, valid until publication.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file(&mut self) -> &mut File {
        self.file.as_mut().expect("a staged file is open until published")
    }

    /// Make the bytes durable, claim `destination`, then make the name durable. Returns the
    /// published file's handle (still open, positioned at its end).
    pub fn publish(self, destination: &Path, claim: Claim) -> Result<File> {
        self.publish_linking(destination, claim, |from, to| std::fs::hard_link(from, to))
    }

    fn publish_linking(
        mut self,
        destination: &Path,
        claim: Claim,
        link: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> Result<File> {
        let context = || format!("publishing {}", destination.display());
        let file = self.file.take().expect("a staged file is published once");
        file.sync_all().with_context(context)?;
        let claimed = match claim {
            Claim::Replace => std::fs::rename(&self.path, destination),
            Claim::New => match link(&self.path, destination) {
                Ok(()) => {
                    // The destination now names the inode; the temporary name is just a second
                    // link, so leaving it behind on failure is harmless.
                    let _ = std::fs::remove_file(&self.path);
                    Ok(())
                }
                Err(e) if links_unsupported(&e) => rename_no_replace(&self.path, destination),
                Err(e) => Err(e),
            },
        };
        match claimed {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(Exists {
                    path: destination.to_owned(),
                    hint: "",
                }
                .into())
            }
            Err(e) => return Err(e).with_context(context),
        }
        // The temporary name is gone (renamed or unlinked); Drop must not remove anything now.
        self.path = PathBuf::new();
        // The name is already claimed. A directory that cannot be synced loses durability of the
        // name, not correctness, so it does not turn a completed publication into a failure.
        sync_parent(destination);
        Ok(file)
    }
}

impl Write for Staged {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file().write(buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.file().write_all(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file().flush()
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        // Close first: some platforms cannot unlink an open file.
        self.file = None;
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Undo a [`Set`] member's claim of a new name. Only the file `published` refers to is removed,
/// never one someone else has put there since: the name is moved to a private one in one step,
/// and what it held is removed only if it is that file; another writer's file is put back without
/// replacing a newer one. Without inode identity (off Unix) nothing can be checked, so nothing is
/// removed and the error says so. `race` runs between looking at the name and taking it.
fn retract_racing(destination: &Path, published: &File, race: impl FnOnce()) -> Result<()> {
    if !holds(destination, published)? {
        return Ok(());
    }
    race();
    // The name can change hands after that look; the rename takes exactly what it holds now.
    let aside = destination.with_file_name(format!(".cv-retract-{}.tmp", uuid::Uuid::new_v4()));
    match std::fs::rename(destination, &aside) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        taken => taken.context("retracting it")?,
    }
    if holds(&aside, published).with_context(|| format!("checking {}", aside.display()))? {
        return std::fs::remove_file(&aside).with_context(|| format!("removing {}", aside.display()));
    }
    rename_no_replace(&aside, destination)
        .with_context(|| format!("another writer's file is kept at {}", aside.display()))
}

/// The filesystem cannot make a hard link here (as opposed to the name being taken or the caller
/// lacking permission to write the directory at all). Linux vfat answers EPERM.
fn links_unsupported(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    if let Some(code) = e.raw_os_error() {
        return [libc::EOPNOTSUPP, libc::ENOTSUP, libc::EPERM, libc::ENOSYS].contains(&code);
    }
    false
}

/// Rename `from` to `to` unless `to` exists, answering `AlreadyExists` if it does.
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    let flag = libc::RENAME_EXCL;
    #[cfg(target_os = "linux")]
    let flag = libc::RENAME_NOREPLACE;
    #[cfg(any(target_vendor = "apple", target_os = "linux"))]
    let exclusive = |from: &Path, to: &Path| rename_with(from, to, flag);
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    let exclusive = |_: &Path, _: &Path| None;
    rename_no_replace_using(from, to, exclusive)
}

fn rename_no_replace_using(
    from: &Path,
    to: &Path,
    exclusive: impl FnOnce(&Path, &Path) -> Option<io::Result<()>>,
) -> io::Result<()> {
    if let Some(result) = exclusive(from, to) {
        return result;
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform has no atomic exclusive rename; destination left untouched",
    ))
}

/// `renamex_np` / `renameat2` with `flag`. A kernel or filesystem that rejects the flag answers
/// `Unsupported`, naming the OS error, so the refusal says which of them declined.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn rename_with(from: &Path, to: &Path, flag: libc::c_uint) -> Option<io::Result<()>> {
    use std::os::unix::ffi::OsStrExt;
    let (from_c, to_c) = match (
        std::ffi::CString::new(from.as_os_str().as_bytes()),
        std::ffi::CString::new(to.as_os_str().as_bytes()),
    ) {
        (Ok(from), Ok(to)) => (from, to),
        (Err(e), _) | (_, Err(e)) => return Some(Err(e.into())),
    };
    // SAFETY: both arguments are NUL-terminated paths that outlive the call.
    #[cfg(target_vendor = "apple")]
    let status = unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), flag) } as i64;
    // SAFETY: as above; renameat2 takes two directory fds, two paths and a flag word.
    #[cfg(target_os = "linux")]
    let status = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            flag,
        )
    } as i64;
    if status == 0 {
        return Some(Ok(()));
    }
    Some(Err(exclusive_refused(io::Error::last_os_error())))
}

/// The OS error of a failed exclusive rename, with a refused flag (`EINVAL`, `ENOSYS`, `ENOTSUP`,
/// `EOPNOTSUPP`: this kernel or filesystem has no exclusive rename) reported as `Unsupported`.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn exclusive_refused(error: io::Error) -> io::Error {
    let unsupported = matches!(
        error.raw_os_error(),
        Some(code) if [libc::EINVAL, libc::ENOSYS, libc::ENOTSUP, libc::EOPNOTSUPP].contains(&code)
    );
    if !unsupported {
        return error;
    }
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("this kernel or filesystem refuses an atomic exclusive rename ({error}); destination left untouched"),
    )
}

/// Publish everything `bytes` yields at `destination` (see [`Staged`]). A new file is created
/// 0666 before the umask; a file it replaces keeps its permission bits. With [`Claim::Replace`]
/// the name moves to the new file: a symlink there is replaced rather than followed, and other
/// hard links keep the old bytes.
pub fn publish_reader(destination: &Path, tag: &str, bytes: impl io::Read, claim: Claim) -> Result<()> {
    stage_reader(destination, tag, bytes, claim)?.publish(destination, claim)?;
    Ok(())
}

/// Stage everything `bytes` yields for `destination`, as [`publish_reader`] publishes it.
pub(crate) fn stage_reader(destination: &Path, tag: &str, mut bytes: impl io::Read, claim: Claim) -> Result<Staged> {
    let replaced = match claim {
        Claim::Replace => std::fs::metadata(destination).ok().filter(|m| m.is_file()),
        Claim::New => None,
    };
    let mut staged = Staged::beside(destination, tag, 0o666)?;
    if let Some(metadata) = replaced {
        staged.file().set_permissions(metadata.permissions())?;
    }
    io::copy(&mut bytes, &mut staged).with_context(|| format!("writing {}", destination.display()))?;
    Ok(staged)
}

/// Several files published as one: every destination takes its new file, or every destination
/// keeps what it held. Members are claimed in the order added, each as [`Staged::publish`] claims
/// it, except that a [`Claim::Replace`] of an existing name keeps the replaced file (swapped into
/// the temporary's name, or moved aside where the platform has no atomic swap, leaving the name
/// briefly absent) until every member holds its name. When a claim fails, every claimed member is
/// undone, last first: its own file is retracted, or the replaced one put back if the name still
/// holds its own. An undo that fails is reported with where each file is, never discarded. A crash
/// midway can still leave a prefix claimed, so add members in the order a reader best tolerates.
#[derive(Default)]
pub struct Set {
    members: Vec<(Staged, PathBuf, Claim)>,
}

/// A member holding its name: its own file, and where the file it replaced is kept until commit.
struct Claimed {
    destination: PathBuf,
    file: File,
    replaced: Option<PathBuf>,
}

impl Set {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stage everything `bytes` yields for `destination`, as [`publish_reader`] would.
    pub fn stage(&mut self, destination: &Path, tag: &str, bytes: impl io::Read, claim: Claim) -> Result<()> {
        let staged = stage_reader(destination, tag, bytes, claim)?;
        self.add(staged, destination, claim);
        Ok(())
    }

    pub fn add(&mut self, staged: Staged, destination: impl Into<PathBuf>, claim: Claim) {
        self.members.push((staged, destination.into(), claim));
    }

    /// Claim every member, or none (see [`Set`]); then remove the files they replaced.
    pub fn publish(self) -> Result<()> {
        self.claim()?.commit()
    }

    /// Claim every member, or none, and keep the files they replaced until the caller decides:
    /// [`Held::commit`] removes them, [`Held::undo`] (or dropping the [`Held`]) puts them back.
    /// This is how a set joins a publication it cannot stage itself: claim, run the other
    /// publication, then commit or undo on its outcome.
    pub fn claim(self) -> Result<Held> {
        self.claim_using(swap, |_| Ok(()))
    }

    fn claim_using(
        self,
        swap: impl Fn(&Path, &Path) -> Option<io::Result<()>>,
        mut before_claim: impl FnMut(usize) -> io::Result<()>,
    ) -> Result<Held> {
        let mut claimed = Vec::with_capacity(self.members.len());
        for (n, (staged, destination, claim)) in self.members.into_iter().enumerate() {
            let result = before_claim(n)
                .map_err(anyhow::Error::from)
                .and_then(|()| claim_member(staged, &destination, claim, &swap));
            match result {
                Ok(member) => claimed.push(member),
                Err(cause) => return Err(undo(claimed, cause)),
            }
        }
        Ok(Held { claimed })
    }
}

/// A [`Set`] whose members all hold their names while the files they replaced are still kept.
/// Dropped undecided, it is undone.
pub struct Held {
    claimed: Vec<Claimed>,
}

impl Held {
    /// Keep the publication: remove the replaced files.
    pub fn commit(mut self) -> Result<()> {
        let kept: Vec<_> = std::mem::take(&mut self.claimed)
            .iter()
            .filter_map(|member| member.replaced.as_ref())
            .filter_map(|old| {
                std::fs::remove_file(old)
                    .err()
                    .map(|e| format!("{}: {e}", old.display()))
            })
            .collect();
        anyhow::ensure!(
            kept.is_empty(),
            "published; removing the replaced files failed ({})",
            kept.join("; ")
        );
        Ok(())
    }

    /// Undo every claim, last first, and return `cause` with every undo that failed.
    pub fn undo(mut self, cause: anyhow::Error) -> anyhow::Error {
        undo(std::mem::take(&mut self.claimed), cause)
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        for member in std::mem::take(&mut self.claimed).iter().rev() {
            let _ = match &member.replaced {
                None => retract_racing(&member.destination, &member.file, || {}),
                Some(old) => restore(&member.destination, &member.file, old),
            };
        }
    }
}

fn claim_member(
    mut staged: Staged,
    destination: &Path,
    claim: Claim,
    swap: impl Fn(&Path, &Path) -> Option<io::Result<()>>,
) -> Result<Claimed> {
    let context = || format!("publishing {}", destination.display());
    let existing = match claim {
        Claim::Replace => match std::fs::symlink_metadata(destination) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).with_context(context),
        },
        Claim::New => None,
    };
    let Some(existing) = existing else {
        let file = staged.publish(destination, claim)?;
        return Ok(Claimed {
            destination: destination.to_owned(),
            file,
            replaced: None,
        });
    };
    anyhow::ensure!(!existing.is_dir(), "{} is a directory", destination.display());
    let file = staged.file.take().expect("a staged file is published once");
    file.sync_all().with_context(context)?;
    let path = std::mem::take(&mut staged.path);
    match exchange(&path, destination, swap, rename_no_replace) {
        Ok(replaced) => {
            sync_parent(destination);
            Ok(Claimed {
                destination: destination.to_owned(),
                file,
                replaced: Some(replaced),
            })
        }
        Err((error, kept)) => {
            if !kept {
                staged.path = path;
            }
            Err(error)
        }
    }
}

/// Undo `claimed`, last first, and return `cause` with every undo that failed.
fn undo(claimed: Vec<Claimed>, cause: anyhow::Error) -> anyhow::Error {
    let failed: Vec<_> = claimed
        .iter()
        .rev()
        .filter_map(|member| {
            let undone = match &member.replaced {
                None => retract_racing(&member.destination, &member.file, || {}),
                Some(old) => restore(&member.destination, &member.file, old),
            };
            undone
                .err()
                .map(|e| format!("{} is left published: {e:#}", member.destination.display()))
        })
        .collect();
    match failed.is_empty() {
        true => cause,
        false => cause.context(format!("undoing the set failed: {}", failed.join("; "))),
    }
}

/// Put `old` back at `destination` if the name still holds `published`; otherwise keep both.
fn restore(destination: &Path, published: &File, old: &Path) -> Result<()> {
    let kept = || format!("the replaced file is kept at {}", old.display());
    anyhow::ensure!(
        holds(destination, published)?,
        "the name holds another file; {}",
        kept()
    );
    std::fs::rename(old, destination).with_context(kept)
}

/// Whether `destination` names `published` itself; off Unix there is no inode to check, so never.
fn holds(destination: &Path, published: &File) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ours = published.metadata()?;
        match std::fs::symlink_metadata(destination) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
            Ok(named) => Ok(named.dev() == ours.dev() && named.ino() == ours.ino()),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (destination, published);
        anyhow::bail!("this platform cannot check that the name still holds the published file")
    }
}

/// A directory built under a private name beside its destination, then published whole: never
/// merged into an existing directory. Dropped unpublished, it is removed.
pub struct StagedDir {
    path: PathBuf,
}

impl StagedDir {
    pub fn beside(destination: &Path, tag: &str) -> Result<Self> {
        let dir = parent_dir(destination);
        let path = dir.join(format!(".cv-{tag}-{}.tmp", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).with_context(|| format!("creating a temporary in {}", dir.display()))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// [`Claim::New`] refuses an existing name. [`Claim::Replace`] swaps the directories in one
    /// step where the platform can (`RENAME_SWAP`, `RENAME_EXCHANGE`); elsewhere it moves the old
    /// one aside first, so the name is briefly absent but never half merged, and a failed claim
    /// puts the old tree back. Every fallback move refuses an existing name, preserving concurrent
    /// claims. If restoration fails, both trees are kept and the error names them.
    /// The replaced tree is then deleted.
    pub fn publish(self, destination: &Path, claim: Claim) -> Result<()> {
        self.publish_using(destination, claim, swap, |_, _| Ok(()))
    }

    fn publish_using(
        mut self,
        destination: &Path,
        claim: Claim,
        swap: impl FnOnce(&Path, &Path) -> Option<io::Result<()>>,
        mut before_move: impl FnMut(&Path, &Path) -> io::Result<()>,
    ) -> Result<()> {
        let rename = |from: &Path, to: &Path| {
            before_move(from, to)?;
            rename_no_replace(from, to)
        };
        let context = || format!("publishing {}", destination.display());
        let exists = std::fs::symlink_metadata(destination).is_ok();
        let staged = std::mem::take(&mut self.path);
        if claim == Claim::New || !exists {
            let claimed = rename_no_replace(&staged, destination);
            if let Err(e) = claimed {
                self.path = staged;
                if e.kind() == io::ErrorKind::AlreadyExists {
                    return Err(Exists {
                        path: destination.to_owned(),
                        hint: "",
                    }
                    .into());
                }
                return Err(e).with_context(context);
            }
            sync_parent(destination);
            return Ok(());
        }
        let old = match exchange(&staged, destination, swap, rename) {
            Ok(old) => old,
            Err((error, kept)) => {
                if !kept {
                    self.path = staged;
                }
                return Err(error);
            }
        };
        sync_parent(destination);
        std::fs::remove_dir_all(&old).with_context(|| format!("removing the replaced {}", old.display()))
    }
}

impl Drop for StagedDir {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// The platform's atomic exchange of two existing names; `None` where it has none.
fn swap(from: &Path, to: &Path) -> Option<io::Result<()>> {
    #[cfg(target_vendor = "apple")]
    return rename_with(from, to, libc::RENAME_SWAP);
    #[cfg(target_os = "linux")]
    return rename_with(from, to, libc::RENAME_EXCHANGE);
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    {
        let _ = (from, to);
        None
    }
}

/// Give the existing `destination` to `staged` and return where the replaced entry now is: the
/// staged name after an atomic swap, else an aside name (the destination is briefly absent, and a
/// failed claim puts the old entry back). Every fallback move refuses an existing name. On failure
/// the flag says whether `staged` still holds the staged entry and may be removed; when putting
/// the old entry back fails too, both are kept and the error names them.
fn exchange(
    staged: &Path,
    destination: &Path,
    swap: impl FnOnce(&Path, &Path) -> Option<io::Result<()>>,
    mut rename: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> std::result::Result<PathBuf, (anyhow::Error, bool)> {
    let failed = |e: io::Error| {
        (
            anyhow::Error::new(e).context(format!("publishing {}", destination.display())),
            false,
        )
    };
    match swap(staged, destination) {
        // The staged name now holds the replaced entry.
        Some(Ok(())) => return Ok(staged.to_owned()),
        Some(Err(e)) => return Err(failed(e)),
        None => {}
    }
    let aside = staged.with_extension("replaced");
    rename(destination, &aside).map_err(failed)?;
    let Err(e) = rename(staged, destination) else {
        return Ok(aside);
    };
    match rename(&aside, destination) {
        Ok(()) => Err(failed(e)),
        // Neither entry holds the name now. Delete nothing; say where both are.
        Err(restore) => Err((
            anyhow::Error::new(e).context(format!(
                "publishing {}; putting the replaced entry back also failed ({restore}), so it is kept at {} \
                 and the new one at {}",
                destination.display(),
                aside.display(),
                staged.display()
            )),
            true,
        )),
    }
}

fn sync_parent(path: &Path) {
    #[cfg(unix)]
    {
        let dir = parent_dir(path);
        if let Ok(dir) = File::open(dir) {
            let _ = dir.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// The directory holding `path` itself: its parent, or `.` for a bare name.
fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Permission bits for an output derived from `source`: no more exposed than what it copies.
pub fn mode_of(source: &File) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        source
            .metadata()
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600)
    }
    #[cfg(not(unix))]
    {
        let _ = source;
        0o666
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn new_claim_fails_closed_without_an_exclusive_rename() {
        let world = cv_testkit::World::new("publish-no-exclusive-rename");
        for directory in [false, true] {
            let from = world.base.join(if directory { "directory" } else { "file" });
            let to = from.with_extension("published");
            if directory {
                std::fs::create_dir(&from).unwrap();
            } else {
                std::fs::write(&from, "complete staged output").unwrap();
            }
            let error = rename_no_replace_using(&from, &to, |_, _| None)
                .expect_err("no exclusive operation means no safe new-name claim");
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert!(from.exists());
            assert!(!to.exists());
        }
    }

    #[test]
    fn a_refused_exclusive_rename_names_the_os_error_not_the_filesystem_alone() {
        let error = rename_no_replace_using(Path::new("a"), Path::new("b"), |_, _| None).unwrap_err();
        assert!(error.to_string().starts_with("this platform has no"), "{error}");
        #[cfg(any(target_vendor = "apple", target_os = "linux"))]
        for code in [libc::EINVAL, libc::ENOTSUP] {
            let error = exclusive_refused(io::Error::from_raw_os_error(code));
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            let os = io::Error::from_raw_os_error(code).to_string();
            assert!(error.to_string().contains(&os), "{error}");
            assert!(error.to_string().contains("kernel or filesystem"), "{error}");
        }
        #[cfg(any(target_vendor = "apple", target_os = "linux"))]
        assert_eq!(
            exclusive_refused(io::Error::from_raw_os_error(libc::EEXIST)).kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn move_aside_replacement_preserves_a_concurrent_directory_claim() {
        let world = cv_testkit::World::new("publish-directory-race");
        let destination = world.base.join("tree");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("old"), "old").unwrap();
        let staged = StagedDir::beside(&destination, "test").unwrap();
        std::fs::write(staged.path().join("new"), "new").unwrap();
        let mut calls = 0;
        let result = staged.publish_using(
            &destination,
            Claim::Replace,
            |_, _| None,
            |_, _| {
                calls += 1;
                if calls == 2 {
                    std::fs::create_dir(&destination)?;
                }
                Ok(())
            },
        );
        assert!(
            result.is_err(),
            "a concurrent claim must block publication and restoration"
        );
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
        let kept = std::fs::read_dir(&world.base)
            .unwrap()
            .filter_map(|e| {
                let path = e.unwrap().path();
                (path.join("old").exists() || path.join("new").exists()).then_some(path)
            })
            .count();
        assert_eq!(kept, 2, "both trees must survive a blocked restoration");
    }

    fn staged(dir: &Path, bytes: &[u8]) -> Staged {
        let mut staged = Staged::create(dir, "test", 0o644).unwrap();
        staged.write_all(bytes).unwrap();
        staged
    }

    fn temporaries(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with(".cv-"))
            .count()
    }

    #[test]
    fn new_refuses_every_existing_name_and_leaves_it_untouched() {
        let world = cv_testkit::World::new("publish-new");
        let dir = &world.base;
        let taken = dir.join("taken");
        std::fs::write(&taken, "before").unwrap();
        let error = staged(dir, b"after").publish(&taken, Claim::New).unwrap_err();
        assert!(error.downcast_ref::<Exists>().is_some(), "{error:#}");
        assert_eq!(std::fs::read(&taken).unwrap(), b"before");
        #[cfg(unix)]
        {
            let dangling = dir.join("dangling");
            std::os::unix::fs::symlink(dir.join("nowhere"), &dangling).unwrap();
            assert!(staged(dir, b"x").publish(&dangling, Claim::New).is_err());
            assert!(!dir.join("nowhere").exists());
        }
        let fresh = dir.join("fresh");
        staged(dir, b"new").publish(&fresh, Claim::New).unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"new");
        assert_eq!(temporaries(dir), 0);
    }

    /// The path a filesystem without hard links takes (FAT32 on macOS answers EOPNOTSUPP, Linux
    /// vfat EPERM): the no-replace rename claims a free name and refuses a taken one.
    #[test]
    fn without_hard_links_new_falls_back_to_a_rename_that_refuses_to_replace() {
        let world = cv_testkit::World::new("publish-nolink");
        let dir = &world.base;
        for code in [
            #[cfg(unix)]
            libc::EOPNOTSUPP,
            #[cfg(unix)]
            libc::EPERM,
        ] {
            let unsupported = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(code));
            let fresh = dir.join(format!("fresh-{code}"));
            staged(dir, b"new")
                .publish_linking(&fresh, Claim::New, unsupported)
                .unwrap();
            assert_eq!(std::fs::read(&fresh).unwrap(), b"new");
            let error = staged(dir, b"other")
                .publish_linking(&fresh, Claim::New, unsupported)
                .unwrap_err();
            assert!(error.downcast_ref::<Exists>().is_some(), "{error:#}");
            assert_eq!(std::fs::read(&fresh).unwrap(), b"new");
        }
        let unsupported = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::Unsupported));
        staged(dir, b"kind")
            .publish_linking(&dir.join("kind"), Claim::New, unsupported)
            .unwrap();
        assert_eq!(temporaries(dir), 0);
    }

    #[test]
    fn replacement_metadata_failure_is_not_absence() {
        let world = cv_testkit::World::new("publish-metadata-error");
        let parent = world.base.join("file");
        std::fs::write(&parent, "original").unwrap();
        let error = claim_member(
            staged(&world.base, b"new"),
            &parent.join("child"),
            Claim::Replace,
            |_, _| panic!("metadata must be checked before claiming"),
        )
        .err()
        .expect("a non-directory parent is an error");
        assert!(format!("{error:#}").contains("publishing"));
        assert_ne!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(std::fs::read(&parent).unwrap(), b"original");
        assert_eq!(temporaries(&world.base), 0);
    }

    #[cfg(unix)]
    #[test]
    fn replace_moves_the_name_and_a_reader_keeps_the_old_bytes() {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        let world = cv_testkit::World::new("publish-replace");
        let dir = &world.base;
        let target = dir.join("target");
        let link = dir.join("link");
        std::fs::write(&target, "before").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut reader = File::open(&target).unwrap();
        let old = reader.metadata().unwrap().ino();
        staged(dir, b"after!").publish(&target, Claim::Replace).unwrap();
        assert_ne!(std::fs::metadata(&target).unwrap().ino(), old);
        assert_eq!(std::fs::read(&target).unwrap(), b"after!");
        let mut held = String::new();
        reader.read_to_string(&mut held).unwrap();
        assert_eq!(held, "before");
        staged(dir, b"via link").publish(&link, Claim::Replace).unwrap();
        assert!(!std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"after!");
        assert_eq!(temporaries(dir), 0);
    }

    /// Without an atomic swap the old tree is moved aside before the new one takes the name. When
    /// that second rename fails the old tree goes back; when putting it back fails too, nothing is
    /// deleted and the error says where both trees are.
    #[test]
    fn a_failed_move_aside_replacement_keeps_a_tree_and_reports_the_error() {
        let world = cv_testkit::World::new("publish-move-aside");
        let tree = |path: &Path| std::fs::read_to_string(path.join("which")).ok();
        // Each case fails the renames whose (1-based) numbers it lists: 1 moves the old tree
        // aside, 2 claims the name, 3 puts the old tree back.
        for (case, failing, published) in [
            ("ok", &[][..], Some("new")),
            ("aside", &[1][..], Some("old")),
            ("claim", &[2][..], Some("old")),
            ("restore", &[2, 3][..], None),
        ] {
            let dir = world.base.join(case);
            std::fs::create_dir(&dir).unwrap();
            let destination = dir.join("tree");
            std::fs::create_dir(&destination).unwrap();
            std::fs::write(destination.join("which"), "old").unwrap();
            let staged = StagedDir::beside(&destination, "test").unwrap();
            std::fs::write(staged.path().join("which"), "new").unwrap();
            let mut calls = 0;
            let rename = |_: &Path, _: &Path| {
                calls += 1;
                match failing.contains(&calls) {
                    true => Err(io::Error::other(format!("injected failure {calls}"))),
                    false => Ok(()),
                }
            };
            let result = staged.publish_using(&destination, Claim::Replace, |_, _| None, rename);
            assert_eq!(result.is_ok(), failing.is_empty(), "{case}: {result:?}");
            assert_eq!(tree(&destination).as_deref(), published, "{case}");
            let left: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p != &destination)
                .collect();
            if published.is_some() {
                assert!(left.is_empty(), "{case}: {left:?}");
                continue;
            }
            // Neither tree holds the name: both are kept, and the error names where.
            let error = format!("{:#}", result.unwrap_err());
            let mut kept: Vec<_> = left.iter().filter_map(|p| tree(p)).collect();
            kept.sort();
            assert_eq!(kept, ["new", "old"], "{case}: {left:?}");
            for path in &left {
                assert!(error.contains(&path.display().to_string()), "{case}: {error}");
            }
            assert!(
                error.contains("injected failure 2") && error.contains("injected failure 3"),
                "{error}"
            );
        }
    }

    /// Make `dir` read-only (or writable again); `false` where that does not bind (running as root).
    #[cfg(unix)]
    pub(crate) fn read_only(dir: &Path, on: bool) -> bool {
        use std::os::unix::fs::PermissionsExt;
        let mode = if on { 0o555 } else { 0o755 };
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
        let probe = dir.join(".cv-probe");
        let binds = !on || std::fs::write(&probe, "").is_err();
        let _ = std::fs::remove_file(&probe);
        binds
    }

    /// The first member of a set is claimed; the second's directory is read-only, so its claim
    /// fails. With an atomic swap and without one, whether the first replaced a file, claimed a
    /// missing name with Replace, or claimed a new one: the first name is back as it was.
    #[cfg(unix)]
    #[test]
    fn a_set_whose_second_claim_fails_publishes_nothing() {
        let world = cv_testkit::World::new("publish-set");
        for swapping in [true, false] {
            for (case, claim, before) in [
                ("replace", Claim::Replace, Some("old")),
                ("missing", Claim::Replace, None),
                ("new", Claim::New, None),
            ] {
                let base = world.base.join(format!("{case}-{swapping}"));
                let (a, b) = (base.join("a"), base.join("b"));
                std::fs::create_dir_all(&a).unwrap();
                std::fs::create_dir_all(&b).unwrap();
                let first = a.join("first");
                if let Some(before) = before {
                    std::fs::write(&first, before).unwrap();
                }
                let mut set = Set::new();
                set.stage(&first, "test", &b"new first"[..], claim).unwrap();
                set.stage(&b.join("second"), "test", &b"new second"[..], Claim::New)
                    .unwrap();
                if !read_only(&b, true) {
                    return;
                }
                let swap = |from: &Path, to: &Path| if swapping { swap(from, to) } else { None };
                let error = set.claim_using(swap, |_| Ok(())).and_then(Held::commit).unwrap_err();
                read_only(&b, false);
                assert!(!format!("{error:#}").contains("undoing"), "{error:#}");
                assert_eq!(std::fs::read_to_string(&first).ok().as_deref(), before, "{case}");
                assert!(!b.join("second").exists());
                assert_eq!(temporaries(&a), 0, "{case}");
            }
        }
        let a = world.base.join("ok");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("first"), "old").unwrap();
        let mut set = Set::new();
        set.stage(&a.join("first"), "test", &b"new first"[..], Claim::Replace)
            .unwrap();
        set.stage(&a.join("second"), "test", &b"new second"[..], Claim::New)
            .unwrap();
        set.publish().unwrap();
        assert_eq!(std::fs::read(a.join("first")).unwrap(), b"new first");
        assert_eq!(std::fs::read(a.join("second")).unwrap(), b"new second");
        assert_eq!(temporaries(&a), 0);
    }

    /// The first member's directory turns read-only after its claim, so undoing it fails too: the
    /// error says so and where the replaced file is, and nothing is deleted.
    #[cfg(unix)]
    #[test]
    fn a_set_reports_an_undo_that_fails() {
        let world = cv_testkit::World::new("publish-set-undo");
        for (case, claim) in [("replace", Claim::Replace), ("new", Claim::New)] {
            let base = world.base.join(case);
            let (a, b) = (base.join("a"), base.join("b"));
            std::fs::create_dir_all(&a).unwrap();
            std::fs::create_dir_all(&b).unwrap();
            let first = a.join("first");
            if claim == Claim::Replace {
                std::fs::write(&first, "old").unwrap();
            }
            let mut set = Set::new();
            set.stage(&first, "test", &b"new first"[..], claim).unwrap();
            set.stage(&b.join("second"), "test", &b"new second"[..], Claim::New)
                .unwrap();
            if !read_only(&b, true) {
                return;
            }
            let error = set
                .claim_using(swap, |n| {
                    if n == 1 {
                        read_only(&a, true);
                    }
                    Ok(())
                })
                .and_then(Held::commit)
                .unwrap_err();
            read_only(&a, false);
            read_only(&b, false);
            let error = format!("{error:#}");
            assert!(error.contains("undoing the set failed"), "{error}");
            assert!(
                error.contains(&format!("{} is left published", first.display())),
                "{error}"
            );
            assert_eq!(std::fs::read(&first).unwrap(), b"new first", "{case}");
            if claim == Claim::Replace {
                let kept = std::fs::read_dir(&a)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| p != &first)
                    .unwrap();
                assert!(error.contains(&format!("kept at {}", kept.display())), "{error}");
                assert_eq!(std::fs::read(&kept).unwrap(), b"old");
            }
        }
    }

    /// Another writer replaces the published name after retract has looked at it: retract still
    /// removes nothing but its own file.
    #[cfg(unix)]
    #[test]
    fn retract_never_removes_a_file_that_replaced_its_own() {
        let world = cv_testkit::World::new("publish-retract-race");
        let path = world.base.join("out");
        let file = staged(&world.base, b"ours").publish(&path, Claim::New).unwrap();
        let replace = || {
            staged(&world.base, b"theirs")
                .publish(&path, Claim::Replace)
                .map(drop)
                .unwrap()
        };
        retract_racing(&path, &file, replace).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"theirs");
        assert_eq!(temporaries(&world.base), 0);
    }

    #[cfg(unix)]
    #[test]
    fn mode_is_kept_and_retract_removes_only_its_own_inode() {
        use std::os::unix::fs::PermissionsExt;
        let world = cv_testkit::World::new("publish-mode");
        let dir = &world.base;
        let path = dir.join("out");
        let mut private = Staged::create(dir, "test", 0o640).unwrap();
        private.write_all(b"x").unwrap();
        let file = private.publish(&path, Claim::New).unwrap();
        let umask_applied = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(umask_applied & !0o640, 0, "{umask_applied:o}");
        assert_eq!(mode_of(&file), umask_applied);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "someone else").unwrap();
        retract_racing(&path, &file, || {}).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"someone else");
        std::fs::remove_file(&path).unwrap();
        let file = staged(dir, b"ours").publish(&path, Claim::New).unwrap();
        retract_racing(&path, &file, || {}).unwrap();
        assert!(!path.exists());
        assert_eq!(temporaries(dir), 0);
    }
}
