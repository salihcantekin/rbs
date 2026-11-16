//! Reading a text source's file (ADR-0129 decision 6): local regular files only, the
//! last 1 MiB at most, UTF-8 or UTF-16 with a byte order mark, and the last N lines.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Most bytes read from a file.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// What the probe learns about a path before reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    /// Length in bytes.
    pub len: u64,
    /// Modification time, at the file system's full precision.
    pub modified: Option<SystemTime>,
}

/// File access of the text worker's probe thread. A trait so tests can make a call
/// that never returns or a file that changes between polls.
pub trait FileProbe: Send + Sync {
    /// The canonical path of `path`, or an error when it does not exist.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    /// The metadata of a canonical path; `Ok(None)` when it is not a regular file.
    fn metadata(&self, path: &Path) -> io::Result<Option<FileMeta>>;
    /// The first `head` bytes and the last `tail` bytes of a regular file (both
    /// capped at its length; they overlap for a short file).
    fn read(&self, path: &Path, head: u64, tail: u64) -> io::Result<(Vec<u8>, Vec<u8>)>;
}

/// The real file system.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealFileProbe;

impl FileProbe for RealFileProbe {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<Option<FileMeta>> {
        let m = std::fs::metadata(path)?;
        if !m.is_file() {
            return Ok(None);
        }
        Ok(Some(FileMeta {
            len: m.len(),
            modified: m.modified().ok(),
        }))
    }

    fn read(&self, path: &Path, head: u64, tail: u64) -> io::Result<(Vec<u8>, Vec<u8>)> {
        let mut f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();
        let mut first = Vec::new();
        (&mut f).take(head.min(len)).read_to_end(&mut first)?;
        let start = len.saturating_sub(tail);
        f.seek(SeekFrom::Start(start))?;
        let mut last = Vec::new();
        f.take(tail.min(len)).read_to_end(&mut last)?;
        Ok((first, last))
    }
}

/// Why a file's content cannot be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTextError {
    /// Not UTF-8, and no UTF-16 byte order mark.
    NotText,
}

/// Decodes the last bytes of a file. `head` is the file's first bytes (for the byte
/// order mark), `tail` its last bytes, `whole` whether `tail` is the whole file.
pub fn decode(head: &[u8], tail: &[u8], whole: bool) -> Result<String, FileTextError> {
    let utf16 = match head {
        [0xFF, 0xFE, ..] => Some(true),
        [0xFE, 0xFF, ..] => Some(false),
        _ => None,
    };
    if let Some(little) = utf16 {
        let mut bytes = tail;
        if whole {
            bytes = &bytes[2.min(bytes.len())..];
        } else if bytes.len() % 2 == 1 {
            // The file's length is even (BOM plus units); an odd tail starts mid-unit.
            bytes = &bytes[1..];
        }
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| {
                if little {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })
            .collect();
        let mut units = units.as_slice();
        // A tail may start on the second half of a surrogate pair.
        if !whole && units.first().is_some_and(|u| (0xDC00..0xE000).contains(u)) {
            units = &units[1..];
        }
        return String::from_utf16(units).map_err(|_| FileTextError::NotText);
    }
    let mut bytes = tail;
    if whole {
        bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    } else {
        // A tail may start inside a character: skip continuation bytes.
        let skip = bytes
            .iter()
            .take(3)
            .take_while(|b| (**b & 0xC0) == 0x80)
            .count();
        bytes = &bytes[skip..];
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| FileTextError::NotText)
}

/// The last `n` lines of `text` (`n` = 0: all of it). Line ends are `\n` or `\r\n`;
/// a final line end does not start an empty line.
pub fn last_lines(text: &str, n: u32) -> String {
    let text = text.replace("\r\n", "\n");
    let trimmed = text.strip_suffix('\n').unwrap_or(&text);
    if n == 0 {
        return trimmed.to_owned();
    }
    let lines: Vec<&str> = trimmed.split('\n').collect();
    let from = lines.len().saturating_sub(n as usize);
    lines[from..].join("\n")
}

/// A 64-bit FNV-1a hash of `bytes`, to see a same-size rewrite.
pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn utf8_with_and_without_bom() {
        assert_eq!(decode(b"abc", b"abc", true).unwrap(), "abc");
        let bom = [&b"\xEF\xBB\xBF"[..], "dünya".as_bytes()].concat();
        assert_eq!(decode(&bom, &bom, true).unwrap(), "dünya");
        // A tail that starts inside "ü".
        let tail = &"dünya".as_bytes()[2..];
        assert_eq!(decode(b"x", tail, false).unwrap(), "nya");
    }

    #[test]
    fn utf16_both_orders() {
        let mut le = vec![0xFF, 0xFE];
        le.extend("İş".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode(&le, &le, true).unwrap(), "İş");
        let mut be = vec![0xFE, 0xFF];
        be.extend("Hi😀".encode_utf16().flat_map(u16::to_be_bytes));
        assert_eq!(decode(&be, &be, true).unwrap(), "Hi😀");
        // A tail that starts on the low surrogate of the emoji and an odd byte.
        let tail = &be[be.len() - 3..];
        assert_eq!(decode(&be, tail, false).unwrap(), "");
    }

    #[test]
    fn invalid_bytes_are_not_text() {
        assert_eq!(
            decode(b"\xC3\x28", b"\xC3\x28", true),
            Err(FileTextError::NotText)
        );
    }

    #[test]
    fn the_last_lines() {
        assert_eq!(last_lines("a\nb\r\nc\n", 0), "a\nb\nc");
        assert_eq!(last_lines("a\nb\r\nc\n", 2), "b\nc");
        assert_eq!(last_lines("a", 5), "a");
        assert_eq!(last_lines("", 3), "");
    }

    #[test]
    fn the_real_probe_reads_head_and_tail() {
        let dir = std::env::temp_dir().join(format!("rbs-text-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.txt");
        std::fs::write(&path, b"0123456789").unwrap();
        let p = RealFileProbe;
        let canon = p.canonicalize(&path).unwrap();
        assert_eq!(p.metadata(&canon).unwrap().unwrap().len, 10);
        assert_eq!(p.metadata(&dir).unwrap(), None);
        let (head, tail) = p.read(&canon, 2, 4).unwrap();
        assert_eq!(
            (head.as_slice(), tail.as_slice()),
            (&b"01"[..], &b"6789"[..])
        );
        assert!(p.canonicalize(&dir.join("missing")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
