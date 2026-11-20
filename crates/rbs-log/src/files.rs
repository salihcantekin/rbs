//! Log file names, segments and pruning (spec `diagnostics-log.md`, "Writer").

#[cfg(unix)]
use std::fs::TryLockError;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

/// How much the logs folder may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Bytes after which a file continues in the next segment.
    pub file: u64,
    /// Runs kept, the current one included.
    pub runs: usize,
    /// Bytes all runs with this prefix may hold together.
    pub total: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            file: 10 * 1024 * 1024,
            runs: 10,
            total: 50 * 1024 * 1024,
        }
    }
}

/// One file of a run, parsed from its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// The start time in the name, `YYYYMMDDTHHMMSSZ`.
    pub time: String,
    /// The process id in the name.
    pub pid: u32,
    /// 0 for the first file, then 1, 2, ...
    pub segment: u32,
}

/// The file name of `part` with `prefix`.
pub fn name(prefix: &str, part: &Part) -> String {
    match part.segment {
        0 => format!("{prefix}-{}-{}.log", part.time, part.pid),
        n => format!("{prefix}-{}-{}.{n}.log", part.time, part.pid),
    }
}

/// The part a file name stands for, when it is exactly `<prefix>-<time>-<pid>[.<n>].log`.
pub fn parse(prefix: &str, file: &str) -> Option<Part> {
    let rest = file.strip_prefix(prefix)?.strip_prefix('-')?;
    let rest = rest.strip_suffix(".log")?;
    let (time, rest) = rest.split_at_checked(16)?;
    let b = time.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if !(digits(0..8) && b[8] == b'T' && digits(9..15) && b[15] == b'Z') {
        return None;
    }
    let rest = rest.strip_prefix('-')?;
    let (pid, segment) = match rest.split_once('.') {
        Some((pid, seg)) => (pid, number(seg)?),
        None => (rest, 0),
    };
    let pid = u32::try_from(number(pid)?).ok()?;
    if segment == 0 && rest.contains('.') {
        return None; // ".0" is never written.
    }
    Some(Part {
        time: time.to_owned(),
        pid,
        segment: u32::try_from(segment).ok()?,
    })
}

fn number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A file found in the folder.
#[derive(Debug, Clone)]
struct Found {
    part: Part,
    path: PathBuf,
    size: u64,
}

/// Removes the oldest runs with `prefix` in `dir` until at most `limits.runs` remain
/// (`current` among them) and all of them hold at most `limits.total` bytes; when only
/// `current` is left over the total, removes its oldest segments. A file that cannot be
/// locked is still written by some process and is kept. Errors are ignored: pruning
/// never stops logging.
pub fn prune(dir: &Path, prefix: &str, current: &Part, limits: &Limits) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut found: Vec<Found> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let part = parse(prefix, e.file_name().to_str()?)?;
            let size = e.metadata().ok()?.len();
            Some(Found {
                part,
                path: e.path(),
                size,
            })
        })
        .collect();
    // Oldest first: by the time in the name, then the pid, then the segment.
    found.sort_by(|a, b| {
        (&a.part.time, a.part.pid, a.part.segment).cmp(&(&b.part.time, b.part.pid, b.part.segment))
    });
    let is_current = |p: &Part| p.time == current.time && p.pid == current.pid;
    let mut runs: Vec<(String, u32)> = Vec::new();
    for f in &found {
        let key = (f.part.time.clone(), f.part.pid);
        if !runs.contains(&key) {
            runs.push(key);
        }
    }
    if !runs
        .iter()
        .any(|(t, p)| *t == current.time && *p == current.pid)
    {
        runs.push((current.time.clone(), current.pid));
    }
    let mut total: u64 = found.iter().map(|f| f.size).sum();
    let mut run_count = runs.len();
    for (time, pid) in runs {
        if run_count <= limits.runs && total <= limits.total {
            return;
        }
        if time == current.time && pid == current.pid {
            continue;
        }
        let mut removed_all = true;
        for f in found
            .iter()
            .filter(|f| f.part.time == time && f.part.pid == pid)
        {
            if remove_unlocked(&f.path) {
                total = total.saturating_sub(f.size);
            } else {
                removed_all = false;
            }
        }
        if removed_all {
            run_count -= 1;
        }
    }
    // Only the current run (and runs in use) left: its oldest segments go, but never
    // the first, which holds the header.
    for f in found
        .iter()
        .filter(|f| is_current(&f.part) && f.part.segment > 0)
    {
        if total <= limits.total {
            return;
        }
        if remove_unlocked(&f.path) {
            total = total.saturating_sub(f.size);
        }
    }
}

/// Removes `path` unless a process still writes it. True when it is gone. On Unix a
/// writer holds an advisory lock (`flock`); on Windows a writer's file is open without
/// delete sharing, so the removal fails while it runs.
fn remove_unlocked(path: &Path) -> bool {
    #[cfg(unix)]
    {
        let Ok(file) = File::open(path) else {
            return false;
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return false,
            // A file system without locks: treat the file as free.
            Err(TryLockError::Error(_)) => {}
        }
    }
    fs::remove_file(path).is_ok()
}

/// Creates `dir/name` (never replacing a file) and marks it as written: on Unix with an
/// exclusive advisory lock, on Windows by opening it without delete sharing (a
/// Windows file lock would stop every other reader, the logs export included).
pub fn create_locked(dir: &Path, name: &str) -> io::Result<File> {
    let mut options = File::options();
    options.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        options.share_mode(FILE_SHARE_READ);
    }
    let file = options.open(dir.join(name))?;
    #[cfg(unix)]
    match file.try_lock() {
        Ok(()) | Err(TryLockError::Error(_)) => {}
        Err(TryLockError::WouldBlock) => return Err(io::ErrorKind::WouldBlock.into()),
    }
    Ok(file)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn part(time: &str, pid: u32, segment: u32) -> Part {
        Part {
            time: time.to_owned(),
            pid,
            segment,
        }
    }

    #[test]
    fn names_round_trip() {
        let p = part("20261120T152735Z", 4182, 0);
        assert_eq!(name("rbs", &p), "rbs-20261120T152735Z-4182.log");
        assert_eq!(parse("rbs", &name("rbs", &p)), Some(p));
        let s = part("20261120T152735Z", 7, 3);
        assert_eq!(name("rbs", &s), "rbs-20261120T152735Z-7.3.log");
        assert_eq!(parse("rbs", &name("rbs", &s)), Some(s));
    }

    #[test]
    fn other_names_do_not_parse() {
        for n in [
            "rbs-browser-20261120T152735Z-1.log",
            "rbs-20261120T152735Z.log",
            "rbs-20261120T152735Z-1.0.log",
            "rbs-20261120T152735Z-1.log.1.log",
            "rbs-2026112XT152735Z-1.log",
            "rbs-20261120T152735Z-1.txt",
            "rbs-20261120T152735Z--1.log",
        ] {
            assert_eq!(parse("rbs", n), None, "{n}");
        }
        assert!(parse("rbs-browser", "rbs-browser-20261120T152735Z-1.log").is_some());
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-log-files-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(d: &Path, name: &str, bytes: usize) {
        fs::write(d.join(name), vec![b'x'; bytes]).unwrap();
    }

    fn names(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn the_oldest_runs_go_first_and_other_prefixes_stay() {
        let d = dir("runs");
        touch(&d, "rbs-20261101T000000Z-1.log", 1);
        touch(&d, "rbs-20261101T000000Z-1.1.log", 1);
        touch(&d, "rbs-20261102T000000Z-2.log", 1);
        touch(&d, "rbs-20261103T000000Z-3.log", 1);
        touch(&d, "rbs-browser-20261101T000000Z-9.log", 1);
        touch(&d, "notes.txt", 1);
        let current = part("20261104T000000Z", 4, 0);
        touch(&d, &name("rbs", &current), 1);
        let limits = Limits {
            file: 100,
            runs: 2,
            total: 1000,
        };
        prune(&d, "rbs", &current, &limits);
        assert_eq!(
            names(&d),
            [
                "notes.txt",
                "rbs-20261103T000000Z-3.log",
                "rbs-20261104T000000Z-4.log",
                "rbs-browser-20261101T000000Z-9.log",
            ]
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_total_removes_old_runs_then_old_segments_of_this_run() {
        let d = dir("total");
        touch(&d, "rbs-20261101T000000Z-1.log", 40);
        let current = part("20261104T000000Z", 4, 3);
        touch(&d, "rbs-20261104T000000Z-4.log", 40);
        touch(&d, "rbs-20261104T000000Z-4.1.log", 40);
        touch(&d, "rbs-20261104T000000Z-4.2.log", 40);
        touch(&d, &name("rbs", &current), 10);
        let limits = Limits {
            file: 40,
            runs: 10,
            total: 100,
        };
        prune(&d, "rbs", &current, &limits);
        assert_eq!(
            names(&d),
            [
                "rbs-20261104T000000Z-4.2.log",
                "rbs-20261104T000000Z-4.3.log",
                "rbs-20261104T000000Z-4.log"
            ]
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_locked_file_is_kept() {
        let d = dir("locked");
        let held = create_locked(&d, "rbs-20261101T000000Z-1.log").unwrap();
        touch(&d, "rbs-20261102T000000Z-2.log", 1);
        let current = part("20261104T000000Z", 4, 0);
        let limits = Limits {
            file: 100,
            runs: 1,
            total: 1000,
        };
        prune(&d, "rbs", &current, &limits);
        assert_eq!(names(&d), ["rbs-20261101T000000Z-1.log"]);
        drop(held);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn create_never_replaces_a_file() {
        let d = dir("create");
        touch(&d, "rbs-20261101T000000Z-1.log", 3);
        assert!(create_locked(&d, "rbs-20261101T000000Z-1.log").is_err());
        let _ = fs::remove_dir_all(&d);
    }
}
