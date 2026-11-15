//! The folder write speed (ADR-0087 decision 4, ADR-0119 decision 4's "synced"
//! write): 256 MiB of incompressible data in 8 MiB chunks, synced, then removed.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rbs_core::time::{Clock, Timestamp};

use super::driver::SkipCode;

/// Bytes the write test writes.
pub const CALIBRATION_WRITE_BYTES: u64 = 256 * 1024 * 1024;

/// Bytes per write call.
pub const CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// A stopped write records a partial rate only from this many synced bytes.
pub const PARTIAL_MIN_BYTES: u64 = 32 * 1024 * 1024;

/// A left-over file of another process is removed from this age.
pub const LEFTOVER_AGE: Duration = Duration::from_secs(10 * 60);

/// The temporary file of process `pid`.
pub fn temp_name(pid: u32) -> String {
    format!(".rbs-calibration-{pid}.tmp")
}

fn pid_of(name: &str) -> Option<u32> {
    name.strip_prefix(".rbs-calibration-")?
        .strip_suffix(".tmp")?
        .parse()
        .ok()
}

/// The file system, injected so tests write nothing.
pub trait WriteSink {
    /// The names in `dir` with their modification time in seconds since the epoch.
    fn list(&mut self, dir: &Path) -> io::Result<Vec<(String, u64)>>;
    /// Removes `path`.
    fn remove(&mut self, path: &Path) -> io::Result<()>;
    /// Creates (truncates) `path` for writing.
    fn create(&mut self, path: &Path) -> io::Result<()>;
    /// Appends `chunk` to the created file.
    fn write(&mut self, chunk: &[u8]) -> io::Result<()>;
    /// Syncs the created file to the device (`F_FULLFSYNC` on macOS) and closes it.
    fn sync(&mut self) -> io::Result<()>;
}

/// The real file system.
#[derive(Debug, Default)]
pub struct FileSink {
    file: Option<std::fs::File>,
}

impl WriteSink for FileSink {
    fn list(&mut self, dir: &Path) -> io::Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if pid_of(&name).is_none() {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            out.push((name, modified));
        }
        Ok(out)
    }

    fn remove(&mut self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn create(&mut self, path: &Path) -> io::Result<()> {
        self.file = Some(std::fs::File::create(path)?);
        Ok(())
    }

    fn write(&mut self, chunk: &[u8]) -> io::Result<()> {
        use std::io::Write as _;
        match &mut self.file {
            Some(f) => f.write_all(chunk),
            None => Err(io::Error::other("no file")),
        }
    }

    fn sync(&mut self) -> io::Result<()> {
        // `sync_all` is `F_FULLFSYNC` on macOS.
        let r = self.file.as_ref().map_or(Ok(()), std::fs::File::sync_all);
        self.file = None;
        r
    }
}

/// What the write test gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderResult {
    /// Bytes per second to the end of the sync; `partial` when a stop ended it early.
    Speed {
        /// The rate.
        bytes_per_second: u64,
        /// Stopped early.
        partial: bool,
    },
    /// No entry.
    Skipped(SkipCode),
}

/// Measures the write speed of `folder`. `free_space` is the disk probe's cached free
/// space (`None` unknown); `now_unix` the wall time in seconds; `deadline` and `stop`
/// end the write between chunks (what was written is synced and, from
/// [`PARTIAL_MIN_BYTES`], recorded as partial). The file is removed in every case.
#[allow(clippy::too_many_arguments)]
pub fn measure_write_speed(
    folder: &Path,
    sink: &mut dyn WriteSink,
    clock: &dyn Clock,
    stop: &AtomicBool,
    deadline: Timestamp,
    bytes: u64,
    free_space: Option<u64>,
    now_unix: u64,
    pid: u32,
) -> FolderResult {
    if free_space.is_some_and(|f| f < bytes.saturating_mul(2)) {
        return FolderResult::Skipped(SkipCode::NoRoom);
    }
    // Left-overs of other processes, older than LEFTOVER_AGE; errors are ignored.
    if let Ok(names) = sink.list(folder) {
        for (name, modified) in names {
            if pid_of(&name).is_some_and(|p| p != pid)
                && now_unix.saturating_sub(modified) >= LEFTOVER_AGE.as_secs()
            {
                let _ = sink.remove(&folder.join(&name));
            }
        }
    }
    let path: PathBuf = folder.join(temp_name(pid));
    let result = write_and_sync(&path, sink, clock, stop, deadline, bytes);
    let _ = sink.remove(&path);
    result
}

fn write_and_sync(
    path: &Path,
    sink: &mut dyn WriteSink,
    clock: &dyn Clock,
    stop: &AtomicBool,
    deadline: Timestamp,
    bytes: u64,
) -> FolderResult {
    if sink.create(path).is_err() {
        return FolderResult::Skipped(SkipCode::WriteFailed);
    }
    let chunk = incompressible(CHUNK_BYTES.min(usize::try_from(bytes).unwrap_or(CHUNK_BYTES)));
    let start = clock.now();
    let mut written = 0u64;
    let mut stopped = None;
    while written < bytes {
        if stop.load(Ordering::Relaxed) {
            stopped = Some(SkipCode::Cancelled);
            break;
        }
        if clock.now() > deadline {
            stopped = Some(SkipCode::TimeLimit);
            break;
        }
        let n = chunk
            .len()
            .min(usize::try_from(bytes - written).unwrap_or(chunk.len()));
        if sink.write(&chunk[..n]).is_err() {
            let _ = sink.sync();
            return FolderResult::Skipped(SkipCode::WriteFailed);
        }
        written += n as u64;
    }
    if sink.sync().is_err() {
        return FolderResult::Skipped(SkipCode::WriteFailed);
    }
    let elapsed = clock.now().saturating_since(start);
    if elapsed.is_zero() || written == 0 {
        return FolderResult::Skipped(stopped.unwrap_or(SkipCode::WriteFailed));
    }
    if let Some(code) = stopped
        && written < PARTIAL_MIN_BYTES
    {
        return FolderResult::Skipped(code);
    }
    FolderResult::Speed {
        bytes_per_second: (written as f64 / elapsed.as_secs_f64()) as u64,
        partial: stopped.is_some(),
    }
}

/// `len` bytes from a seeded generator, so compressing or deduplicating file systems
/// do not flatter the result.
fn incompressible(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let take = (len - out.len()).min(8);
        out.extend_from_slice(&state.to_le_bytes()[..take]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_names_carry_the_pid() {
        assert_eq!(pid_of(&temp_name(42)), Some(42));
        assert_eq!(pid_of("other.tmp"), None);
    }

    #[test]
    fn the_data_does_not_repeat_in_short_runs() {
        let d = incompressible(4096);
        assert_eq!(d.len(), 4096);
        assert_ne!(d[..8], d[8..16]);
    }
}
