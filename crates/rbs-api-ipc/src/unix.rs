//! Unix domain socket endpoint: a socket of mode 0600 in a directory only the current
//! user can use (mode 0700, owned by the user, not a symbolic link), and the peer's
//! user checked with the OS (`SO_PEERCRED` on Linux, `getpeereid` elsewhere).

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::conn::{Stream, WRITE_STALL};

/// The effective user id of this process.
fn current_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// The user id the peer of `s` runs as, from the OS.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the descriptor is a connected Unix socket owned by `s` for the call;
    // `cred` and `len` are valid for writes and `len` holds `cred`'s size.
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    (r == 0 && len as usize == std::mem::size_of::<libc::ucred>()).then_some(cred.uid)
}

/// The user id the peer of `s` runs as, from the OS.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: the descriptor is a connected Unix socket owned by `s` for the call;
    // `uid` and `gid` are valid for writes.
    let r = unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) };
    (r == 0).then_some(uid)
}

pub(crate) fn default_endpoint() -> Result<PathBuf, String> {
    let var = |name| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let base = if cfg!(target_os = "macos") {
        var("TMPDIR")
    } else {
        var("XDG_RUNTIME_DIR")
    };
    Ok(match base {
        Some(dir) => dir.join("rbs").join("control.sock"),
        None => PathBuf::from(format!("/tmp/rbs-{}", current_uid())).join("control.sock"),
    })
}

/// Makes `dir` if missing, then checks it is a real directory of the current user
/// that nobody else can enter.
fn private_dir(dir: &Path) -> Result<(), String> {
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("cannot make {}: {e}", dir.display())),
    }
    let m = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !m.file_type().is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    if m.uid() != current_uid() {
        return Err(format!(
            "{} belongs to another user, so it is not used",
            dir.display()
        ));
    }
    if m.mode() & 0o077 != 0 {
        return Err(format!(
            "other users can use {} (mode {:o}), so it is not used",
            dir.display(),
            m.mode() & 0o777
        ));
    }
    Ok(())
}

pub(crate) struct Listener {
    inner: UnixListener,
    path: PathBuf,
    /// Device and inode of the socket made, so `remove` never deletes another's.
    id: (u64, u64),
    uid: u32,
    /// `wake` writes to one end; `accept` polls the other with the listener, so
    /// waking needs nothing from the file system.
    wake_rx: UnixStream,
    wake_tx: UnixStream,
    /// Held while serving: a second server on the same endpoint is refused before it
    /// touches the socket file.
    _lock: std::fs::File,
}

impl Listener {
    pub(crate) fn bind(path: &Path) -> Result<Listener, String> {
        let dir = path
            .parent()
            .ok_or_else(|| "the endpoint has no directory".to_owned())?;
        private_dir(dir)?;
        let mut name = path
            .file_name()
            .ok_or_else(|| "the endpoint has no file name".to_owned())?
            .to_owned();
        name.push(".lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join(name))
            .map_err(|e| e.to_string())?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err("another program already serves this endpoint".into());
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.to_string()),
        }
        // Under the lock, a socket file is one a process that ended left behind.
        if std::fs::symlink_metadata(path).is_ok() {
            std::fs::remove_file(path).map_err(|e| e.to_string())?;
        }
        let inner = UnixListener::bind(path).map_err(|e| e.to_string())?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        // A connection that vanishes between the poll and the accept must not block.
        inner.set_nonblocking(true).map_err(|e| e.to_string())?;
        let m = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        let (wake_rx, wake_tx) = UnixStream::pair().map_err(|e| e.to_string())?;
        wake_tx.set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(Listener {
            inner,
            path: path.to_owned(),
            id: (m.dev(), m.ino()),
            uid: current_uid(),
            wake_rx,
            wake_tx,
            _lock: lock,
        })
    }

    /// Waits for the listener (when `listener`) or the wake-up, at most `timeout_ms`
    /// (-1: no limit). True when a connection may be waiting; false when woken or
    /// timed out. A wake-up stays set: every later wait returns at once.
    fn wait(&self, timeout_ms: i32, listener: bool) -> io::Result<bool> {
        let mut fds = [
            libc::pollfd {
                fd: self.wake_rx.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.inner.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = if listener { 2 } else { 1 };
        loop {
            // SAFETY: `fds` holds `n` initialised entries whose descriptors stay open
            // (owned by `self`) for the call.
            let r = unsafe { libc::poll(fds.as_mut_ptr(), n, timeout_ms) };
            if r >= 0 {
                break;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        if fds[0].revents != 0 {
            return Ok(false);
        }
        Ok(listener && fds[1].revents != 0)
    }

    /// The next connection: `Ok(None)` when woken, or for a peer of another user
    /// (closed at once).
    pub(crate) fn accept(&self) -> io::Result<Option<Box<dyn Stream>>> {
        if !self.wait(-1, true)? {
            return Ok(None);
        }
        let s = match self.inner.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        };
        // On macOS and the BSDs the accepted socket inherits non-blocking mode.
        s.set_nonblocking(false)?;
        if peer_uid(&s) != Some(self.uid) {
            return Ok(None);
        }
        // Fails (EINVAL on macOS) when the peer has already gone: drop it.
        if s.set_write_timeout(Some(WRITE_STALL)).is_err() {
            return Ok(None);
        }
        // The reader's wake-up: `shutdown` alone does not reliably end a `read`
        // another thread is blocked in on macOS, so the reader waits on this too.
        let Ok((wake_rx, wake_tx)) = UnixStream::pair() else {
            return Ok(None);
        };
        if wake_tx.set_nonblocking(true).is_err() {
            return Ok(None);
        }
        Ok(Some(Box::new(UnixIo {
            s,
            wake_rx,
            wake_tx,
        })))
    }

    /// Makes a blocked `accept` (or `wait_after_error`) return; the stop flag is set
    /// first.
    pub(crate) fn wake(&self) {
        let _ = (&self.wake_tx).write(&[1]);
    }

    /// After an accept error (out of descriptors): pause briefly instead of spinning;
    /// a wake-up ends the pause (the wake byte is never read, so the accept that
    /// follows returns at once and the loop sees the stop flag).
    pub(crate) fn wait_after_error(&self) -> bool {
        let _ = self.wait(100, false);
        true
    }

    /// Removes the socket file, if it is still the one made here.
    pub(crate) fn remove(&self) {
        if let Ok(m) = std::fs::symlink_metadata(&self.path)
            && (m.dev(), m.ino()) == self.id
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// An accepted connection. `read` waits for the socket or the wake-up, so
/// `shutdown` ends a read blocked on the reader thread on every Unix: on macOS
/// `shutdown` on a Unix socket does not always wake a thread already in `read`
/// (the conformance suite's "server closes" check then never saw the close).
struct UnixIo {
    s: UnixStream,
    /// Polled with the socket; one byte from `shutdown` makes every later read end.
    wake_rx: UnixStream,
    wake_tx: UnixStream,
}

impl UnixIo {
    /// Waits until the socket has something to read (or hung up): false when woken.
    fn wait_readable(&self) -> io::Result<bool> {
        let mut fds = [
            libc::pollfd {
                fd: self.wake_rx.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.s.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: `fds` holds two initialised entries whose descriptors are owned
            // by `self` and stay open for the call.
            let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if r >= 0 {
                break;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        Ok(fds[0].revents == 0)
    }
}

impl Stream for UnixIo {
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.wait_readable()? {
            // Shut down: the end of the stream.
            return Ok(0);
        }
        (&self.s).read(buf)
    }

    fn write_all(&self, buf: &[u8]) -> io::Result<()> {
        (&self.s).write_all(buf)
    }

    fn shutdown(&self) {
        // The byte is never read, so the wake-up stays set.
        let _ = (&self.wake_tx).write(&[1]);
        let _ = self.s.shutdown(Shutdown::Both);
    }
}

/// The client side: a connection to `path` whose server runs as the current user.
pub(crate) struct ClientIo(UnixStream);

impl ClientIo {
    pub(crate) fn connect(path: &Path, _timeout: Duration) -> Result<ClientIo, crate::ClientError> {
        let s = UnixStream::connect(path).map_err(crate::ClientError::NotRunning)?;
        if peer_uid(&s) != Some(current_uid()) {
            return Err(crate::ClientError::WrongServer);
        }
        Ok(ClientIo(s))
    }

    pub(crate) fn write_all(&self, buf: &[u8]) -> io::Result<()> {
        (&self.0).write_all(buf)
    }

    /// Reads what is there, waiting at most `timeout` (`None`: no limit). `Ok(None)`
    /// when nothing came in time.
    pub(crate) fn read(
        &self,
        buf: &mut [u8],
        timeout: Option<Duration>,
    ) -> io::Result<Option<usize>> {
        if let Err(e) = self
            .0
            .set_read_timeout(timeout.map(|t| t.max(Duration::from_millis(1))))
        {
            // macOS refuses socket options (EINVAL) once the server has shut the
            // connection down; what it sent before is still there to read.
            if e.kind() != io::ErrorKind::InvalidInput {
                return Err(e);
            }
            self.0.set_nonblocking(true)?;
            let r = (&self.0).read(buf);
            let _ = self.0.set_nonblocking(false);
            return match r {
                Ok(n) => Ok(Some(n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(Some(0)),
                Err(e) => Err(e),
            };
        }
        match (&self.0).read(buf) {
            Ok(n) => Ok(Some(n)),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    pub(crate) fn shutdown_write(&self) {
        let _ = self.0.shutdown(Shutdown::Write);
    }
}
