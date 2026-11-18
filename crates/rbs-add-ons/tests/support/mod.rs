//! In-memory fetch and archive, and a file system wrapper that can fail on cue.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::ops::ControlFlow;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_add_ons::{
    AddOnFs, ArchiveEntry, Entry, EntryKind, Fetch, FetchError, Format, FsError, LockGuard,
    RelPath, Response, Unpack, UnpackError, parse_entry,
};
use rbs_core::time::TestClock;
use sha2::{Digest, Sha256};

pub const TARGET: &str = "x86_64-unknown-linux-gnu";
pub const URL: &str = "https://cdn.example.org/runtime.tar.gz";

// ---------- Archive ----------

/// One entry of a test archive.
#[derive(Clone)]
pub struct Item {
    pub kind: EntryKind,
    pub name: Vec<u8>,
    pub link: Option<Vec<u8>>,
    pub mode: u32,
    pub data: Vec<u8>,
}

/// Builds the bytes [`MemUnpack`] reads.
#[derive(Default, Clone)]
pub struct Archive(pub Vec<Item>);

impl Archive {
    pub fn new() -> Self {
        Self::default()
    }
    fn push(
        mut self,
        kind: EntryKind,
        name: &[u8],
        link: Option<&[u8]>,
        mode: u32,
        data: &[u8],
    ) -> Self {
        self.0.push(Item {
            kind,
            name: name.to_vec(),
            link: link.map(<[u8]>::to_vec),
            mode,
            data: data.to_vec(),
        });
        self
    }
    pub fn dir(self, name: &str) -> Self {
        self.push(EntryKind::Dir, name.as_bytes(), None, 0o755, b"")
    }
    pub fn file(self, name: &str, data: &[u8]) -> Self {
        self.push(EntryKind::File, name.as_bytes(), None, 0o644, data)
    }
    pub fn file_mode(self, name: &str, mode: u32) -> Self {
        self.push(EntryKind::File, name.as_bytes(), None, mode, b"x")
    }
    pub fn raw_file(self, name: &[u8]) -> Self {
        self.push(EntryKind::File, name, None, 0o644, b"x")
    }
    pub fn symlink(self, name: &str, target: &str) -> Self {
        self.push(
            EntryKind::Symlink,
            name.as_bytes(),
            Some(target.as_bytes()),
            0o777,
            b"",
        )
    }
    pub fn hard_link(self, name: &str, target: &str) -> Self {
        self.push(
            EntryKind::HardLink,
            name.as_bytes(),
            Some(target.as_bytes()),
            0o644,
            b"",
        )
    }
    pub fn other(self, name: &str) -> Self {
        self.push(EntryKind::Other, name.as_bytes(), None, 0o644, b"")
    }

    /// A minimal runtime: `top/Release/libcef.so`, `top/Resources/icudtl.dat`.
    pub fn runtime() -> Self {
        Self::new()
            .dir("top/")
            .dir("top/Release/")
            .file("top/Release/libcef.so", b"library")
            .dir("top/Resources/")
            .file("top/Resources/icudtl.dat", b"icu")
            .file("top/Resources/resources.pak", b"pak")
    }

    pub fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for item in &self.0 {
            let kind = match item.kind {
                EntryKind::File => 0u8,
                EntryKind::Dir => 1,
                EntryKind::Symlink => 2,
                EntryKind::HardLink => 3,
                EntryKind::Other => 4,
            };
            out.push(kind);
            put(&mut out, &item.name);
            put(&mut out, item.link.as_deref().unwrap_or(b"\0none"));
            out.extend_from_slice(&item.mode.to_le_bytes());
            put(&mut out, &item.data);
        }
        out
    }
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn take(input: &[u8], at: &mut usize, n: usize) -> Result<Vec<u8>, UnpackError> {
    let end = at
        .checked_add(n)
        .filter(|e| *e <= input.len())
        .ok_or_else(|| UnpackError("truncated".into()))?;
    let v = input[*at..end].to_vec();
    *at = end;
    Ok(v)
}

fn take_u32(input: &[u8], at: &mut usize) -> Result<u32, UnpackError> {
    let b = take(input, at, 4)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Decodes [`Archive::bytes`].
pub struct MemUnpack;

impl Unpack for MemUnpack {
    fn unpack(
        &self,
        _format: Format,
        input: &mut dyn Read,
        visit: &mut dyn FnMut(ArchiveEntry<'_>) -> ControlFlow<()>,
    ) -> Result<(), UnpackError> {
        let mut all = Vec::new();
        input
            .read_to_end(&mut all)
            .map_err(|e| UnpackError(e.to_string()))?;
        let mut at = 0;
        while at < all.len() {
            let kind = match take(&all, &mut at, 1)?[0] {
                0 => EntryKind::File,
                1 => EntryKind::Dir,
                2 => EntryKind::Symlink,
                3 => EntryKind::HardLink,
                _ => EntryKind::Other,
            };
            let n = take_u32(&all, &mut at)? as usize;
            let name = take(&all, &mut at, n)?;
            let n = take_u32(&all, &mut at)? as usize;
            let link = take(&all, &mut at, n)?;
            let mode = take_u32(&all, &mut at)?;
            let n = take_u32(&all, &mut at)? as usize;
            let data = take(&all, &mut at, n)?;
            let mut reader: &[u8] = &data;
            let entry = ArchiveEntry {
                name,
                kind,
                mode,
                link: (link != b"\0none").then_some(link),
                data: &mut reader,
            };
            if visit(entry).is_break() {
                return Ok(());
            }
        }
        Ok(())
    }
}

// ---------- Catalog ----------

pub fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A runtime entry for `archive` with `version`.
pub fn entry_for(archive: &[u8], version: &str) -> Entry {
    entry_with(archive, version, &[])
}

/// As [`entry_for`], with `(from, to)` text replacements in the TOML.
pub fn entry_with(archive: &[u8], version: &str, edits: &[(&str, &str)]) -> Entry {
    let mut text = format!(
        r#"
id = "org.rbs.browser-engine"
kind = "runtime"
name = "Browser engine"
summary = "Lets browser sources show web pages."
version = "{version}"
hosts = ["cdn.example.org", "assets.example.org"]
notices = "https://cdn.example.org/notices"

[platform.{TARGET}]
url = "{URL}"
size = {size}
sha256 = "{sha}"
unpacked_size = 4096
format = "tar.gz"
strip = 1
library_dir = "Release"
resources_dir = "Resources"
required = ["libcef.so", "icudtl.dat"]
"#,
        size = archive.len(),
        sha = sha_hex(archive),
    );
    for (from, to) in edits {
        assert!(text.contains(from), "{from}");
        text = text.replace(from, to);
    }
    parse_entry(&text, TARGET).unwrap().unwrap()
}

// ---------- Fetch ----------

#[derive(Clone)]
pub enum Answer {
    Redirect(String),
    Body(Vec<u8>),
    Fail(FetchError),
    /// Sends these bytes, then stalls.
    Stall(Vec<u8>),
}

#[derive(Default)]
pub struct MemFetch {
    pub answers: Mutex<HashMap<String, Answer>>,
    pub requests: Mutex<Vec<String>>,
    /// Set after the first body read (to cancel mid-download).
    pub after_first_read: Mutex<Option<Arc<AtomicBool>>>,
    /// Advanced by each body read.
    pub clock_per_read: Mutex<Option<(TestClock, Duration)>>,
    /// Bytes per body read.
    pub chunk: Mutex<Option<usize>>,
}

impl MemFetch {
    pub fn serving(url: &str, bytes: &[u8]) -> Self {
        let f = Self::default();
        f.answer(url, Answer::Body(bytes.to_vec()));
        f
    }
    pub fn answer(&self, url: &str, answer: Answer) {
        self.answers.lock().unwrap().insert(url.to_owned(), answer);
    }
    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

struct Body {
    data: Vec<u8>,
    at: usize,
    stall: bool,
    flag: Option<Arc<AtomicBool>>,
    clock: Option<(TestClock, Duration)>,
    chunk: usize,
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(flag) = self.flag.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some((clock, step)) = &self.clock {
            clock.advance(*step);
        }
        if self.at >= self.data.len() {
            if self.stall {
                return Err(io::Error::other(FetchError::Stalled));
            }
            return Ok(0);
        }
        let n = buf.len().min(self.chunk).min(self.data.len() - self.at);
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

impl Fetch for MemFetch {
    fn get(&self, url: &str) -> Result<Response, FetchError> {
        self.requests.lock().unwrap().push(url.to_owned());
        let answer = self.answers.lock().unwrap().get(url).cloned();
        let (data, stall) = match answer {
            None => return Err(FetchError::Network(format!("404 {url}"))),
            Some(Answer::Redirect(to)) => return Ok(Response::Redirect(to)),
            Some(Answer::Fail(e)) => return Err(e),
            Some(Answer::Body(b)) => (b, false),
            Some(Answer::Stall(b)) => (b, true),
        };
        Ok(Response::Body(Box::new(Body {
            data,
            at: 0,
            stall,
            flag: self.after_first_read.lock().unwrap().clone(),
            clock: self.clock_per_read.lock().unwrap().clone(),
            chunk: self.chunk.lock().unwrap().unwrap_or(usize::MAX),
        })))
    }
}

// ---------- File system ----------

struct Guard {
    path: PathBuf,
    held: Arc<Mutex<HashSet<PathBuf>>>,
}

impl LockGuard for Guard {}

impl Drop for Guard {
    fn drop(&mut self) {
        self.held.lock().unwrap().remove(&self.path);
    }
}

/// `std::fs` in a temporary directory, with faults on cue and an operation log.
pub struct TestFs {
    pub held: Arc<Mutex<HashSet<PathBuf>>>,
    /// Paths "another process" holds.
    pub other_process: Mutex<HashSet<PathBuf>>,
    pub free: Mutex<u64>,
    pub sharing_violations: AtomicU32,
    pub pauses: AtomicU32,
    pub clock: TestClock,
    pub log: Mutex<Vec<String>>,
}

impl TestFs {
    pub fn new(clock: TestClock) -> Self {
        Self {
            held: Arc::default(),
            other_process: Mutex::default(),
            free: Mutex::new(u64::MAX),
            sharing_violations: AtomicU32::new(0),
            pauses: AtomicU32::new(0),
            clock,
            log: Mutex::default(),
        }
    }

    fn note(&self, op: &str, path: &Path) {
        self.log
            .lock()
            .unwrap()
            .push(format!("{op} {}", path.display()));
    }

    /// Refuses when any existing component under `root` is a link.
    fn beneath(root: &Path, rel: &RelPath) -> Result<PathBuf, FsError> {
        let mut path = root.to_path_buf();
        for c in rel.components() {
            path.push(c);
            if path
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                return Err(FsError::Link);
            }
        }
        Ok(path)
    }
}

fn io_err(e: io::Error) -> FsError {
    if e.kind() == io::ErrorKind::NotFound {
        FsError::NotFound
    } else {
        FsError::Io(e.to_string())
    }
}

impl AddOnFs for TestFs {
    fn lock(&self, path: &Path) -> Result<Box<dyn LockGuard>, FsError> {
        if self.other_process.lock().unwrap().contains(path)
            || !self.held.lock().unwrap().insert(path.to_path_buf())
        {
            return Err(FsError::Busy);
        }
        Ok(Box::new(Guard {
            path: path.to_path_buf(),
            held: Arc::clone(&self.held),
        }))
    }
    fn create_file_beneath(&self, root: &Path, rel: &RelPath, mode: u32) -> Result<File, FsError> {
        let path = Self::beneath(root, rel)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(io_err)?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(io_err)?;
        Ok(file)
    }
    fn create_dir_beneath(&self, root: &Path, rel: &RelPath) -> Result<(), FsError> {
        let path = Self::beneath(root, rel)?;
        std::fs::create_dir_all(path).map_err(io_err)
    }
    fn symlink_beneath(&self, root: &Path, rel: &RelPath, target: &str) -> Result<(), FsError> {
        let path = Self::beneath(root, rel)?;
        std::os::unix::fs::symlink(target, path).map_err(io_err)
    }
    fn hard_link_beneath(
        &self,
        root: &Path,
        rel: &RelPath,
        existing: &RelPath,
    ) -> Result<(), FsError> {
        let path = Self::beneath(root, rel)?;
        let existing = Self::beneath(root, existing)?;
        std::fs::hard_link(existing, path).map_err(io_err)
    }
    fn open_read(&self, path: &Path) -> Result<File, FsError> {
        File::open(path).map_err(io_err)
    }
    fn sync_file(&self, _file: &File) -> Result<(), FsError> {
        Ok(())
    }
    fn sync_dir(&self, _dir: &Path) -> Result<(), FsError> {
        Ok(())
    }
    fn free_space(&self, _dir: &Path) -> Result<u64, FsError> {
        Ok(*self.free.lock().unwrap())
    }
    fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        // Faults only on the move of the unpacked tree into its version directory.
        let left = self.sharing_violations.load(Ordering::Relaxed);
        if left > 0 && from.ends_with("out") {
            self.sharing_violations.store(left - 1, Ordering::Relaxed);
            return Err(FsError::SharingViolation);
        }
        self.note("rename", to);
        std::fs::rename(from, to).map_err(io_err)
    }
    fn remove_file(&self, path: &Path) -> Result<(), FsError> {
        self.note("remove_file", path);
        std::fs::remove_file(path).map_err(io_err)
    }
    fn remove_all(&self, dir: &Path) -> Result<(), FsError> {
        if dir.symlink_metadata().is_err() {
            return Ok(());
        }
        self.note("remove_all", dir);
        std::fs::remove_dir_all(dir).map_err(io_err)
    }
    fn list_dir(&self, dir: &Path) -> Result<Vec<String>, FsError> {
        match std::fs::read_dir(dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(io_err(e)),
            Ok(rd) => Ok(rd
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()),
        }
    }
    fn exists(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }
    fn pause(&self, duration: Duration) {
        self.pauses.fetch_add(1, Ordering::Relaxed);
        self.clock.advance(duration);
    }
}
