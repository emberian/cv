//! What wakes a resident follower: the kernel's word that a watched directory changed, instead of
//! a blind sleep. A follower that blocks on directory notifications wakes immediately when a
//! session file is appended to or created, falling back to the caller's poll interval otherwise
//! ([`super::Watcher`] still decides what is new).
//!
//! On Linux this is `inotify` plus an `eventfd` (so another thread can wake the loop early via
//! [`Waker`]). Elsewhere on native targets it is a [`std::sync::Condvar`] wait at the caller's
//! fallback interval, reporting [`Woke::overflow`] on every wake.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Why [`Wake::wait`] returned.
#[derive(Debug, Default)]
pub struct Woke {
    /// Files and directories that changed in watched directories (a new directory included).
    pub paths: Vec<PathBuf>,
    /// The kernel dropped events (its queue overflowed) or this platform cannot say what
    /// changed: treat every followed session as possibly changed.
    pub overflow: bool,
    /// A [`Waker`] fired.
    pub woken: bool,
    /// The timeout passed with nothing else.
    pub timed_out: bool,
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::ffi::{CString, OsString};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::sync::Arc;

    const MASK: u32 = libc::IN_MODIFY | libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE;

    pub struct Wake {
        inotify: OwnedFd,
        event: Arc<OwnedFd>,
        dirs: HashMap<i32, PathBuf>,
        watched: HashSet<PathBuf>,
    }

    /// Wakes a [`Wake`] from another thread.
    #[derive(Clone)]
    pub struct Waker(Arc<OwnedFd>);

    impl Waker {
        pub fn wake(&self) {
            let one: u64 = 1;
            // SAFETY: writing 8 bytes from a live u64 to an eventfd we own.
            let _ = unsafe { libc::write(self.0.as_raw_fd(), std::ptr::from_ref(&one).cast(), 8) };
        }
    }

    fn owned(fd: i32, what: &str) -> anyhow::Result<OwnedFd> {
        anyhow::ensure!(fd >= 0, "{what}: {}", std::io::Error::last_os_error());
        // SAFETY: a fresh descriptor the kernel just returned, owned from here on.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    impl Wake {
        pub fn new(_fallback: Duration) -> anyhow::Result<Wake> {
            // SAFETY: plain syscalls with constant flags.
            let inotify = owned(
                unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) },
                "inotify_init1",
            )?;
            let event = owned(
                unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) },
                "eventfd",
            )?;
            Ok(Wake {
                inotify,
                event: Arc::new(event),
                dirs: HashMap::new(),
                watched: HashSet::new(),
            })
        }

        pub fn waker(&self) -> Waker {
            Waker(self.event.clone())
        }

        pub fn watch(&mut self, dir: &Path) -> anyhow::Result<bool> {
            if self.watched.contains(dir) {
                return Ok(false);
            }
            let c = CString::new(dir.as_os_str().as_bytes())?;
            // SAFETY: a NUL-terminated path and our inotify descriptor.
            let wd = unsafe { libc::inotify_add_watch(self.inotify.as_raw_fd(), c.as_ptr(), MASK | libc::IN_ONLYDIR) };
            anyhow::ensure!(
                wd >= 0,
                "watching {}: {}",
                dir.display(),
                std::io::Error::last_os_error()
            );
            self.dirs.insert(wd, dir.to_owned());
            self.watched.insert(dir.to_owned());
            Ok(true)
        }

        pub fn watched(&self) -> usize {
            self.watched.len()
        }

        pub fn wait(&mut self, timeout: Option<Duration>) -> anyhow::Result<Woke> {
            let mut fds = [
                libc::pollfd {
                    fd: self.inotify.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.event.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let ms = timeout.map_or(-1, |t| i32::try_from(t.as_millis()).unwrap_or(i32::MAX));
            // SAFETY: two initialized pollfd entries.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
            let mut woke = Woke::default();
            if n < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    return Ok(woke);
                }
                return Err(error.into());
            }
            if n == 0 {
                woke.timed_out = true;
                return Ok(woke);
            }
            if fds[1].revents != 0 {
                let mut count = 0u64;
                // SAFETY: reading 8 bytes into a live u64.
                let _ = unsafe { libc::read(self.event.as_raw_fd(), std::ptr::from_mut(&mut count).cast(), 8) };
                woke.woken = true;
            }
            if fds[0].revents != 0 {
                self.drain(&mut woke)?;
            }
            Ok(woke)
        }

        fn drain(&mut self, woke: &mut Woke) -> anyhow::Result<()> {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                // SAFETY: reading into our buffer of its own length.
                let n = unsafe { libc::read(self.inotify.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::WouldBlock {
                        return Ok(());
                    }
                    return Err(error.into());
                }
                if n == 0 {
                    return Ok(());
                }
                let n = n.cast_unsigned();
                let header = std::mem::size_of::<libc::inotify_event>();
                let mut at = 0;
                while at + header <= n {
                    // SAFETY: the kernel wrote whole events; read_unaligned copes with alignment.
                    let ev: libc::inotify_event = unsafe { std::ptr::read_unaligned(buf[at..].as_ptr().cast()) };
                    let name_len = ev.len as usize;
                    let name = &buf[at + header..(at + header + name_len).min(n)];
                    let name = &name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())];
                    at += header + name_len;
                    if ev.mask & libc::IN_Q_OVERFLOW != 0 {
                        woke.overflow = true;
                        continue;
                    }
                    if ev.mask & libc::IN_IGNORED != 0 {
                        if let Some(dir) = self.dirs.remove(&ev.wd) {
                            self.watched.remove(&dir);
                        }
                        continue;
                    }
                    if let Some(dir) = self.dirs.get(&ev.wd) {
                        let path = if name.is_empty() {
                            dir.clone()
                        } else {
                            dir.join(OsString::from_vec(name.to_vec()))
                        };
                        if !woke.paths.contains(&path) {
                            woke.paths.push(path);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(all(not(target_os = "linux"), not(target_family = "wasm")))]
mod imp {
    use super::*;
    use std::sync::{Arc, Condvar, Mutex};

    pub struct Wake {
        fallback: Duration,
        signal: Arc<(Mutex<bool>, Condvar)>,
    }

    /// Wakes a [`Wake`] from another thread.
    #[derive(Clone)]
    pub struct Waker(Arc<(Mutex<bool>, Condvar)>);

    impl Waker {
        pub fn wake(&self) {
            let (flag, cv) = &*self.0;
            *flag.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            cv.notify_all();
        }
    }

    impl Wake {
        pub fn new(fallback: Duration) -> anyhow::Result<Wake> {
            Ok(Wake {
                fallback,
                signal: Default::default(),
            })
        }

        pub fn waker(&self) -> Waker {
            Waker(self.signal.clone())
        }

        pub fn watch(&mut self, _dir: &Path) -> anyhow::Result<bool> {
            Ok(false)
        }

        pub fn watched(&self) -> usize {
            0
        }

        pub fn wait(&mut self, timeout: Option<Duration>) -> anyhow::Result<Woke> {
            let wait = timeout.map_or(self.fallback, |t| t.min(self.fallback));
            let (flag, cv) = &*self.signal;
            let guard = flag.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let (mut guard, _) = cv
                .wait_timeout_while(guard, wait, |fired| !*fired)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let woken = std::mem::take(&mut *guard);
            Ok(Woke {
                paths: Vec::new(),
                overflow: true,
                woken,
                timed_out: !woken,
            })
        }
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    pub struct Wake {
        fired: Arc<AtomicBool>,
    }

    /// Wakes a [`Wake`].
    #[derive(Clone)]
    pub struct Waker(Arc<AtomicBool>);

    impl Waker {
        pub fn wake(&self) {
            self.0.store(true, Ordering::Release);
        }
    }

    impl Wake {
        pub fn new(_fallback: Duration) -> anyhow::Result<Wake> {
            Ok(Wake {
                fired: Arc::new(AtomicBool::new(false)),
            })
        }

        pub fn waker(&self) -> Waker {
            Waker(self.fired.clone())
        }

        pub fn watch(&mut self, _dir: &Path) -> anyhow::Result<bool> {
            Ok(false)
        }

        pub fn watched(&self) -> usize {
            0
        }

        pub fn wait(&mut self, _timeout: Option<Duration>) -> anyhow::Result<Woke> {
            let woken = self.fired.swap(false, Ordering::AcqRel);
            Ok(Woke {
                paths: Vec::new(),
                overflow: true,
                woken,
                timed_out: !woken,
            })
        }
    }
}

pub use imp::{Wake, Waker};

impl Wake {
    /// Watch every existing root directory of `harness` (all built-in harnesses when `None`).
    pub fn watch_roots(&mut self, harness: Option<crate::Harness>) -> Vec<PathBuf> {
        let adapters: Vec<_> = match harness {
            Some(h) => crate::harness::for_harness(h).into_iter().collect(),
            None => crate::harness::all(),
        };
        let mut roots = Vec::new();
        for adapter in adapters {
            for root in adapter.roots() {
                let _ = self.watch(&root);
                roots.push(root);
            }
        }
        roots
    }

    /// Watch `path`'s parent directory chain up to any enclosing entry in `roots` (or `path`'s
    /// parent alone when outside them), so writes to `path` and newly created sibling directories
    /// wake.
    pub fn watch_path(&mut self, roots: &[PathBuf], path: &Path) {
        let Some(parent) = path.parent() else {
            return;
        };
        let root = roots.iter().find(|root| parent.starts_with(root));
        let mut dir = parent.to_path_buf();
        loop {
            let _ = self.watch(&dir);
            if Some(&dir) == root || root.is_none() || !dir.pop() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waker_interrupts_wait_and_timeout_reports_timed_out() {
        let mut wake = Wake::new(Duration::from_secs(5)).unwrap();
        let timed_out = wake.wait(Some(Duration::from_millis(10))).unwrap();
        assert!(timed_out.timed_out);
        assert!(!timed_out.woken);

        let waker = wake.waker();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            waker.wake();
        });
        let started = std::time::Instant::now();
        let woke = wake.wait(Some(Duration::from_secs(5))).unwrap();
        assert!(woke.woken);
        assert!(!woke.timed_out);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_write_in_a_watched_directory_names_the_file_and_a_waker_wakes() {
        let dir = std::env::temp_dir().join(format!("cv-wake-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut wake = Wake::new(Duration::from_secs(1)).unwrap();
        assert!(wake.watch(&dir).unwrap());
        assert!(!wake.watch(&dir).unwrap());
        let woke = wake.wait(Some(Duration::from_millis(10))).unwrap();
        assert!(woke.timed_out && woke.paths.is_empty());
        let file = dir.join("s.jsonl");
        std::fs::write(&file, b"{}\n").unwrap();
        let woke = wake.wait(Some(Duration::from_secs(5))).unwrap();
        assert!(woke.paths.contains(&file), "{woke:?}");
        let waker = wake.waker();
        std::thread::spawn(move || waker.wake());
        let woke = wake.wait(Some(Duration::from_secs(5))).unwrap();
        assert!(woke.woken);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
