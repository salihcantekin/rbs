//! Save logs (spec `diagnostics-log.md`, "Settings › Advanced"): the last runs, the
//! crash reports and a few summary files in one zip a user can attach to a bug report.
//!
//! The zip is written by hand with stored (uncompressed) entries: a log is small next
//! to the 50 MB the folder may hold, and this keeps the crate free of an archive
//! dependency. Every reader (Finder, Explorer, `unzip`, GitHub) opens it.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::files;

/// Crash reports a bundle takes at most, newest first.
pub const CRASH_REPORTS: usize = 10;

/// What goes into the zip.
#[derive(Debug, Clone)]
pub struct Bundle {
    /// The logs folder.
    pub logs: PathBuf,
    /// The file name prefixes whose runs are taken (`rbs`, `rbs-browser`).
    pub prefixes: Vec<String>,
    /// Runs taken per prefix, newest first.
    pub runs: usize,
    /// The crash reports folder, when there is one.
    pub crash_reports: Option<PathBuf>,
    /// More files, by name in the zip, with their text (a system summary).
    pub extra: Vec<(String, String)>,
}

/// Writes `bundle` to `out` (replacing it) and returns how many files it holds. A file
/// that cannot be read is left out; only `out` failing is an error.
pub fn write_zip(bundle: &Bundle, out: &Path, now: SystemTime) -> io::Result<usize> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for prefix in &bundle.prefixes {
        for path in newest_runs(&bundle.logs, prefix, bundle.runs) {
            if let (Some(name), Ok(bytes)) = (file_name(&path), fs::read(&path)) {
                entries.push((format!("logs/{name}"), bytes));
            }
        }
    }
    if let Some(dir) = &bundle.crash_reports {
        for path in newest_reports(dir, CRASH_REPORTS) {
            if let (Some(name), Ok(bytes)) = (file_name(&path), fs::read(&path)) {
                entries.push((format!("crash-reports/{name}"), bytes));
            }
        }
    }
    for (name, text) in &bundle.extra {
        entries.push((name.clone(), text.as_bytes().to_vec()));
    }
    let mut w = BufWriter::new(File::create(out)?);
    zip(&mut w, &entries, dos_time(now))?;
    w.flush()?;
    Ok(entries.len())
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()?.to_str().map(str::to_owned)
}

/// The files of the newest `runs` runs with `prefix`, oldest file first.
fn newest_runs(dir: &Path, prefix: &str, runs: usize) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(files::Part, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| Some((files::parse(prefix, e.file_name().to_str()?)?, e.path())))
        .collect();
    found.sort_by(|(a, _), (b, _)| (&a.time, a.pid, a.segment).cmp(&(&b.time, b.pid, b.segment)));
    let mut keys: Vec<(String, u32)> = Vec::new();
    for (p, _) in found.iter().rev() {
        let key = (p.time.clone(), p.pid);
        if !keys.contains(&key) {
            if keys.len() == runs {
                break;
            }
            keys.push(key);
        }
    }
    found
        .into_iter()
        .filter(|(p, _)| keys.contains(&(p.time.clone(), p.pid)))
        .map(|(_, path)| path)
        .collect()
}

/// The newest `n` `.txt` reports in `dir`, by modification time.
fn newest_reports(dir: &Path, n: usize) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "txt"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file()
                .then(|| (meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), e.path()))
        })
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.truncate(n);
    found.into_iter().map(|(_, p)| p).collect()
}

/// MS-DOS date and time (UTC) for the zip headers.
fn dos_time(now: SystemTime) -> (u16, u16) {
    let (y, m, d, h, min, s, _) = crate::time::civil(now);
    // DOS dates start in 1980.
    let year = y.clamp(1980, 2107) - 1980;
    let date = (year << 9) | (m << 5) | d;
    let time = (h << 11) | (min << 5) | (s / 2);
    (
        u16::try_from(time).unwrap_or(0),
        u16::try_from(date).unwrap_or(0),
    )
}

/// A zip of stored entries (PKWARE APPNOTE 4.3: local headers, then the central
/// directory). Sizes stay under 4 GiB: the logs folder holds 50 MB.
fn zip(
    w: &mut impl Write,
    entries: &[(String, Vec<u8>)],
    (time, date): (u16, u16),
) -> io::Result<()> {
    let too_big = || io::Error::new(io::ErrorKind::InvalidInput, "the logs are too large");
    let mut central = Vec::new();
    let mut offset: u32 = 0;
    let count = u16::try_from(entries.len()).map_err(|_| too_big())?;
    for (name, data) in entries {
        let crc = crc32(data);
        let size = u32::try_from(data.len()).map_err(|_| too_big())?;
        let name_len = u16::try_from(name.len()).map_err(|_| too_big())?;
        let mut local = Vec::with_capacity(30 + name.len());
        local.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
        local.extend_from_slice(&20_u16.to_le_bytes()); // version needed
        local.extend_from_slice(&0x0800_u16.to_le_bytes()); // UTF-8 names
        local.extend_from_slice(&0_u16.to_le_bytes()); // stored
        local.extend_from_slice(&time.to_le_bytes());
        local.extend_from_slice(&date.to_le_bytes());
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&name_len.to_le_bytes());
        local.extend_from_slice(&0_u16.to_le_bytes()); // extra
        local.extend_from_slice(name.as_bytes());
        w.write_all(&local)?;
        w.write_all(data)?;

        central.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
        central.extend_from_slice(&20_u16.to_le_bytes()); // made by
        central.extend_from_slice(&20_u16.to_le_bytes()); // needed
        central.extend_from_slice(&0x0800_u16.to_le_bytes());
        central.extend_from_slice(&0_u16.to_le_bytes());
        central.extend_from_slice(&time.to_le_bytes());
        central.extend_from_slice(&date.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&name_len.to_le_bytes());
        central.extend_from_slice(&[0; 8]); // extra, comment, disk, internal attributes
        central.extend_from_slice(&0_u32.to_le_bytes()); // external attributes
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
        let len = u32::try_from(local.len() + data.len()).map_err(|_| too_big())?;
        offset = offset.checked_add(len).ok_or_else(too_big)?;
    }
    let central_len = u32::try_from(central.len()).map_err(|_| too_big())?;
    w.write_all(&central)?;
    let mut end = Vec::with_capacity(22);
    end.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
    end.extend_from_slice(&[0; 4]); // disk numbers
    end.extend_from_slice(&count.to_le_bytes());
    end.extend_from_slice(&count.to_le_bytes());
    end.extend_from_slice(&central_len.to_le_bytes());
    end.extend_from_slice(&offset.to_le_bytes());
    end.extend_from_slice(&0_u16.to_le_bytes()); // comment
    w.write_all(&end)
}

/// CRC-32 (IEEE), bit by bit: a bundle is written once, on a user's click.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use std::time::Duration;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-log-export-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn crc32_matches_the_reference_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn the_newest_runs_and_reports_and_the_extras_go_in() {
        let d = dir("bundle");
        let logs = d.join("logs");
        let crash = d.join("crash");
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(&crash).unwrap();
        for n in [
            "rbs-20261101T000000Z-1.log",
            "rbs-20261102T000000Z-2.log",
            "rbs-20261102T000000Z-2.1.log",
            "rbs-20261103T000000Z-3.log",
            "other.log",
        ] {
            fs::write(logs.join(n), n).unwrap();
        }
        fs::write(crash.join("panic-1-2-0.txt"), "boom").unwrap();
        fs::write(crash.join("running-9.lock"), "").unwrap();
        let bundle = Bundle {
            logs: logs.clone(),
            prefixes: vec!["rbs".into()],
            runs: 2,
            crash_reports: Some(crash),
            extra: vec![("system.txt".into(), "RBS 0.0.1".into())],
        };
        let out = d.join("logs.zip");
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_795_188_455);
        assert_eq!(write_zip(&bundle, &out, at).unwrap(), 5);
        let zip = fs::read(&out).unwrap();
        let text = String::from_utf8_lossy(&zip);
        for name in [
            "logs/rbs-20261102T000000Z-2.log",
            "logs/rbs-20261102T000000Z-2.1.log",
            "logs/rbs-20261103T000000Z-3.log",
            "crash-reports/panic-1-2-0.txt",
            "system.txt",
        ] {
            assert!(text.contains(name), "{name}");
        }
        assert!(!text.contains("rbs-20261101T000000Z-1.log"));
        assert!(!text.contains("running-9.lock"));
        // The end record: five entries, and the central directory where it says.
        let end = &zip[zip.len() - 22..];
        assert_eq!(&end[..4], &0x0605_4b50_u32.to_le_bytes());
        assert_eq!(u16::from_le_bytes([end[10], end[11]]), 5);
        let cd_len = u32::from_le_bytes([end[12], end[13], end[14], end[15]]) as usize;
        let cd_at = u32::from_le_bytes([end[16], end[17], end[18], end[19]]) as usize;
        assert_eq!(cd_at + cd_len, zip.len() - 22);
        assert_eq!(&zip[cd_at..cd_at + 4], &0x0201_4b50_u32.to_le_bytes());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn dos_time_is_utc_and_counts_from_1980() {
        // 2026-11-20 15:27:35 UTC.
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_795_188_455);
        let (time, date) = dos_time(at);
        assert_eq!(date >> 9, 2026 - 1980);
        assert_eq!((date >> 5) & 0xf, 11);
        assert_eq!(date & 0x1f, 20);
        assert_eq!(time >> 11, 15);
        assert_eq!((time >> 5) & 0x3f, 27);
    }
}
