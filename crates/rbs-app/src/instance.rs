//! One RBS per settings folder (ADR-0181).
//!
//! `rbs` takes an exclusive lock on `instance.lock` in its settings folder before
//! anything else. A launch that cannot take it asks the running RBS to show its main
//! window (Windows: a named event, macOS: a reopen) and ends; while the running RBS is
//! closing or not answering yet, it waits for the lock for at most [`WAIT`].
//!
//! The parts here are the same on every system and are tested on every system: the
//! lock, the folder key the Windows event is named by, the macOS state file and the
//! wait. The signals themselves live in `win` and `native`.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The lock file in the settings folder.
pub const LOCK_FILE: &str = "instance.lock";

/// What the running RBS is doing (macOS reads it; ADR-0181 decision 2).
pub const STATE_FILE: &str = "instance.state";

/// How long a launch waits for a closing or silent RBS: the shutdown budget
/// (`shutdown::STEPS`, 12 s) plus a margin (ADR-0181 decision 4).
pub const WAIT: Duration = Duration::from_secs(15);

/// How often a waiting launch tries to signal again.
pub const RETRY: Duration = Duration::from_millis(500);

/// The text a launch shows when the running RBS did not answer in time.
pub const NO_ANSWER: &str = "RBS is already running but did not respond. Try again in a moment.";

/// Held by the one RBS of a settings folder; the system releases it when the process
/// ends, however it ends.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

/// The result of [`claim`].
#[derive(Debug)]
pub enum Claim {
    /// This process is the RBS of the folder.
    Owner(InstanceLock),
    /// Another process holds the folder.
    Taken,
    /// The lock could not be used for another reason (a network home without locks, a
    /// read-only folder): RBS starts as before and logs why (decision 1).
    Unavailable(io::Error),
}

/// Takes the lock of `folder`, creating the folder when needed. The file is never
/// truncated, so a launch does not change what the running RBS wrote.
pub fn claim(folder: &Path) -> Claim {
    let file = match open_lock(folder) {
        Ok(f) => f,
        Err(e) => return Claim::Unavailable(e),
    };
    match file.try_lock() {
        Ok(()) => Claim::Owner(InstanceLock { _file: file }),
        Err(TryLockError::WouldBlock) => Claim::Taken,
        Err(TryLockError::Error(e)) => Claim::Unavailable(e),
    }
}

fn open_lock(folder: &Path) -> io::Result<File> {
    fs::create_dir_all(folder)?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(folder.join(LOCK_FILE))
}

/// How a waiting launch ended.
#[derive(Debug)]
pub enum Waited {
    /// The lock came free: this process is now the RBS of the folder.
    Owner(InstanceLock),
    /// The running RBS took the signal; this launch ends.
    Answered,
    /// Neither within the time allowed.
    TimedOut,
    /// The lock could not be used after all; RBS starts as before.
    Unavailable(io::Error),
}

/// Waits up to `limit` for the lock of `folder` (ADR-0181 decision 4). `signal` is
/// tried at once and every `retry` while waiting, and ends the wait when it reports a
/// delivered signal. The blocking lock runs on a helper thread; a launch that ends
/// leaves it behind, and the process end releases it.
pub fn wait_for(
    folder: &Path,
    limit: Duration,
    retry: Duration,
    mut signal: impl FnMut() -> bool,
) -> Waited {
    let file = match open_lock(folder) {
        Ok(f) => f,
        Err(e) => return Waited::Unavailable(e),
    };
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("rbs-instance-wait".into())
        .spawn(move || {
            let locked = file.lock().map(|()| file);
            let _ = tx.send(locked);
        });
    if let Err(e) = spawned {
        return Waited::Unavailable(e);
    }
    let end = Instant::now() + limit;
    loop {
        if signal() {
            return Waited::Answered;
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Waited::TimedOut;
        }
        match rx.recv_timeout(left.min(retry)) {
            Ok(Ok(file)) => return Waited::Owner(InstanceLock { _file: file }),
            Ok(Err(e)) => return Waited::Unavailable(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Waited::Unavailable(io::ErrorKind::Other.into());
            }
        }
    }
}

/// The key the Windows show event is named by: the 64-bit FNV-1a hash of the canonical
/// folder path, lower-cased, as 16 hex digits. Stable across builds, unlike std's
/// hasher, so an older and a newer build agree.
pub fn folder_key(folder: &Path) -> String {
    let path = fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    key_of(&path.to_string_lossy())
}

fn key_of(path: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// What the running RBS of a folder is doing, as `instance.state` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// No state, or one that cannot be read: the RBS is starting.
    Starting,
    /// Its reopen handler exists; it can be asked to show its window.
    Running {
        /// Its process id.
        pid: u32,
        /// The executable it runs, so a reused pid is never asked.
        exe: PathBuf,
    },
    /// Quit was decided; it is ending.
    Closing {
        /// Its process id.
        pid: u32,
        /// The executable it runs.
        exe: PathBuf,
    },
}

impl State {
    fn line(&self) -> Option<String> {
        let (word, pid, exe) = match self {
            Self::Starting => return None,
            Self::Running { pid, exe } => ("running", pid, exe),
            Self::Closing { pid, exe } => ("closing", pid, exe),
        };
        Some(format!("{word} {pid} {}\n", exe.display()))
    }

    /// Reads a state line; anything partial or unknown is [`State::Starting`].
    pub fn parse(text: &str) -> Self {
        let Some(line) = text.strip_suffix('\n') else {
            return Self::Starting;
        };
        let mut parts = line.splitn(3, ' ');
        let (Some(word), Some(pid), Some(exe)) = (parts.next(), parts.next(), parts.next()) else {
            return Self::Starting;
        };
        let (Ok(pid), false) = (pid.parse::<u32>(), exe.is_empty()) else {
            return Self::Starting;
        };
        let exe = PathBuf::from(exe);
        match word {
            "running" => Self::Running { pid, exe },
            "closing" => Self::Closing { pid, exe },
            _ => Self::Starting,
        }
    }
}

/// Reads the state of `folder`.
pub fn read_state(folder: &Path) -> State {
    fs::read_to_string(folder.join(STATE_FILE)).map_or(State::Starting, |t| State::parse(&t))
}

/// Writes the state of this RBS whole (a temporary file, then a rename); `Starting`
/// removes the file. Only the lock holder calls it.
pub fn write_state(_lock: &InstanceLock, folder: &Path, state: &State) -> io::Result<()> {
    let path = folder.join(STATE_FILE);
    let Some(line) = state.line() else {
        return match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    };
    let tmp = folder.join(format!("{STATE_FILE}.tmp"));
    let mut file = File::create(&tmp)?;
    file.write_all(line.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rbs-instance-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn owner(c: Claim) -> InstanceLock {
        match c {
            Claim::Owner(l) => l,
            other => panic!("expected the lock, got {other:?}"),
        }
    }

    #[test]
    fn a_second_claim_waits_for_the_first_to_end() {
        let dir = folder("claim");
        let first = owner(claim(&dir));
        assert!(matches!(claim(&dir), Claim::Taken));
        drop(first);
        let _again = owner(claim(&dir));
    }

    #[test]
    fn a_claim_never_truncates_the_lock_file() {
        let dir = folder("keep");
        let _first = owner(claim(&dir));
        fs::write(dir.join(LOCK_FILE), b"x").ok();
        assert!(matches!(claim(&dir), Claim::Taken));
        // Readable on Unix (advisory lock); on Windows the range is locked.
        if cfg!(unix) {
            assert_eq!(fs::read(dir.join(LOCK_FILE)).unwrap(), b"x");
        }
    }

    #[test]
    fn an_unusable_folder_is_unavailable_not_taken() {
        let dir = folder("file");
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        fs::write(&dir, b"a file where the folder should be").unwrap();
        assert!(matches!(claim(&dir), Claim::Unavailable(_)));
        let _ = fs::remove_file(&dir);
    }

    #[test]
    fn the_wait_ends_when_the_signal_is_delivered() {
        let dir = folder("answered");
        let _first = owner(claim(&dir));
        let mut tries = 0;
        let waited = wait_for(
            &dir,
            Duration::from_secs(10),
            Duration::from_millis(1),
            || {
                tries += 1;
                tries == 3
            },
        );
        assert!(matches!(waited, Waited::Answered));
        assert_eq!(tries, 3);
    }

    #[test]
    fn the_wait_takes_the_lock_when_it_comes_free() {
        let dir = folder("free");
        let first = owner(claim(&dir));
        let mut first = Some(first);
        // The first try releases the lock, as a closing RBS ending does.
        let waited = wait_for(
            &dir,
            Duration::from_secs(10),
            Duration::from_secs(10),
            || {
                first.take();
                false
            },
        );
        assert!(matches!(waited, Waited::Owner(_)));
    }

    #[test]
    fn the_wait_gives_up_after_its_limit() {
        let dir = folder("timeout");
        let _first = owner(claim(&dir));
        let waited = wait_for(&dir, Duration::ZERO, Duration::ZERO, || false);
        assert!(matches!(waited, Waited::TimedOut));
    }

    #[test]
    fn the_folder_key_is_stable_and_ignores_case() {
        assert_eq!(
            key_of("C:\\Users\\Z\\AppData\\Roaming\\RBS"),
            key_of("c:\\users\\z\\appdata\\roaming\\rbs")
        );
        // FNV-1a 64 of the empty string: the offset basis.
        assert_eq!(key_of(""), "cbf29ce484222325");
        assert_eq!(key_of("a"), "af63dc4c8601ec8c");
        assert_ne!(key_of("/a/rbs"), key_of("/b/rbs"));
    }

    #[test]
    fn two_spellings_of_one_folder_have_one_key() {
        let dir = folder("spell");
        fs::create_dir_all(dir.join("sub")).unwrap();
        let roundabout = dir.join("sub").join("..");
        assert_eq!(folder_key(&dir), folder_key(&roundabout));
    }

    #[test]
    fn the_state_round_trips_and_partial_reads_as_starting() {
        let exe = PathBuf::from("/Applications/RBS.app/Contents/MacOS/rbs");
        for s in [
            State::Running {
                pid: 42,
                exe: exe.clone(),
            },
            State::Closing {
                pid: 7,
                exe: exe.clone(),
            },
        ] {
            assert_eq!(State::parse(&s.line().unwrap()), s);
        }
        for partial in [
            "",
            "running",
            "running 42",
            "running 42 /x",
            "running x /x\n",
            "running 42 \n",
            "other 1 /x\n",
        ] {
            assert_eq!(State::parse(partial), State::Starting, "{partial:?}");
        }
    }

    #[test]
    fn the_state_file_is_written_whole_and_removed_for_starting() {
        let dir = folder("state");
        let lock = owner(claim(&dir));
        assert_eq!(read_state(&dir), State::Starting);
        let running = State::Running {
            pid: 9,
            exe: PathBuf::from("/x/rbs"),
        };
        write_state(&lock, &dir, &running).unwrap();
        assert_eq!(read_state(&dir), running);
        write_state(&lock, &dir, &State::Starting).unwrap();
        assert_eq!(read_state(&dir), State::Starting);
        write_state(&lock, &dir, &State::Starting).unwrap();
    }
}
