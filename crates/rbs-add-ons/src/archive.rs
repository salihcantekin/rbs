//! Unpacking: the decoder is behind [`Unpack`]; every rule is here ([add-ons spec],
//! "Transfer and install", steps 6 and 7).
//!
//! Each entry is checked by its name in the archive alone. Links are created last,
//! and a link target is resolved through the archive's own links, never through the
//! disk, so an archive cannot reach outside the output directory by creating a link
//! first and writing through it later.
//!
//! [add-ons spec]: https://github.com/salihcantekin/rbs/blob/main/docs/specs/add-ons.md

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{Read, Write};
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::catalog::Artifact;
use crate::fs::{AddOnFs, FsError};
use crate::names::{NameSet, RelPath, check_component};
use crate::status::InstallError;
use crate::store::MARKER;

/// Most entries one archive may have.
pub(crate) const MAX_ENTRIES: usize = 20_000;
/// Most links followed while resolving one link target.
pub(crate) const MAX_LINK_HOPS: usize = 16;
/// Copy buffer size.
const CHUNK: usize = 64 * 1024;

/// Archive formats the catalog may name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A gzip-compressed tar file.
    TarGz,
    /// A bzip2-compressed tar file (upstream CEF builds).
    TarBz2,
}

/// The type of one archive entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link; [`ArchiveEntry::link`] is its text.
    Symlink,
    /// A hard link; [`ArchiveEntry::link`] is the name of an earlier entry.
    HardLink,
    /// Anything else (devices, FIFOs, ...): always refused.
    Other,
}

/// One entry, as the decoder reads it. Nothing is on disk yet.
pub struct ArchiveEntry<'a> {
    /// The name exactly as stored.
    pub name: Vec<u8>,
    /// The type.
    pub kind: EntryKind,
    /// The stored Unix mode bits.
    pub mode: u32,
    /// Link text for links.
    pub link: Option<Vec<u8>>,
    /// The content of a regular file.
    pub data: &'a mut dyn Read,
}

/// A decoder failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnpackError(pub String);

impl fmt::Display for UnpackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot read the archive: {}", self.0)
    }
}

impl std::error::Error for UnpackError {}

/// Decodes an archive. It never writes to disk and checks nothing; it calls `visit`
/// for each entry in archive order and stops when `visit` breaks.
pub trait Unpack: Send + Sync {
    /// Reads `input` as `format`.
    fn unpack(
        &self,
        format: Format,
        input: &mut dyn Read,
        visit: &mut dyn FnMut(ArchiveEntry<'_>) -> ControlFlow<()>,
    ) -> Result<(), UnpackError>;
}

#[derive(Debug, Clone)]
enum Node {
    /// A directory created by an entry or implied by a deeper one.
    Dir,
    File,
    /// Components of the link text (`.` and `..` kept).
    Symlink(Vec<String>),
    HardLink,
}

/// Unpacks into `out` and places the `required` files. On error the caller empties
/// staging; nothing here is ever used without the marker.
pub(crate) fn extract(
    fs: &dyn AddOnFs,
    unpack: &dyn Unpack,
    artifact: &Artifact,
    input: &mut dyn Read,
    out: &Path,
    cancel: &AtomicBool,
) -> Result<(), InstallError> {
    let mut x = Extract {
        fs,
        out,
        artifact,
        cancel,
        names: NameSet::default(),
        nodes: HashMap::new(),
        symlinks: Vec::new(),
        hard_links: Vec::new(),
        entries: 0,
        written: 0,
        error: None,
    };
    let decoded = unpack.unpack(artifact.format, input, &mut |entry| match x.entry(entry) {
        Ok(()) => ControlFlow::Continue(()),
        Err(e) => {
            x.error = Some(e);
            ControlFlow::Break(())
        }
    });
    if let Some(e) = x.error.take() {
        return Err(e);
    }
    decoded.map_err(|e| InstallError::UnsafeArchive(e.to_string()))?;
    x.links()?;
    x.place_required()
}

struct Extract<'a> {
    fs: &'a dyn AddOnFs,
    out: &'a Path,
    artifact: &'a Artifact,
    cancel: &'a AtomicBool,
    names: NameSet,
    nodes: HashMap<RelPath, Node>,
    symlinks: Vec<(RelPath, String)>,
    hard_links: Vec<(RelPath, RelPath)>,
    entries: usize,
    written: u64,
    error: Option<InstallError>,
}

fn unsafe_archive(why: impl Into<String>) -> InstallError {
    InstallError::UnsafeArchive(why.into())
}

fn io(e: FsError) -> InstallError {
    match e {
        FsError::Link => unsafe_archive("a path component is a link on disk"),
        other => InstallError::Io(other.to_string()),
    }
}

impl Extract<'_> {
    fn entry(&mut self, entry: ArchiveEntry<'_>) -> Result<(), InstallError> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(InstallError::Cancelled);
        }
        self.entries += 1;
        if self.entries > MAX_ENTRIES {
            return Err(unsafe_archive(format!("more than {MAX_ENTRIES} entries")));
        }
        let raw = RelPath::from_bytes(&entry.name).map_err(unsafe_archive)?;
        let Some(path) = raw.strip(usize::from(self.artifact.strip)) else {
            // The leading directories that `strip` removes.
            return match entry.kind {
                EntryKind::Dir => Ok(()),
                _ => Err(unsafe_archive(format!(
                    "`{raw}` is above the stripped directory"
                ))),
            };
        };
        if path.components() == [MARKER] {
            return Err(unsafe_archive(format!("`{MARKER}` is reserved")));
        }
        self.names.insert(&path).map_err(unsafe_archive)?;
        self.check_parents(&path)?;
        match self.nodes.get(&path) {
            None => {}
            // An explicit directory entry after a deeper entry implied it.
            Some(Node::Dir) if entry.kind == EntryKind::Dir => {}
            Some(_) => return Err(unsafe_archive(format!("`{path}` appears twice"))),
        }
        match entry.kind {
            EntryKind::Dir => {
                self.fs.create_dir_beneath(self.out, &path).map_err(io)?;
                self.nodes.insert(path, Node::Dir);
            }
            EntryKind::File => {
                self.write_file(&path, entry.mode, entry.data)?;
                self.nodes.insert(path, Node::File);
            }
            EntryKind::Symlink => {
                let text = entry
                    .link
                    .as_deref()
                    .ok_or_else(|| unsafe_archive(format!("`{path}`: link without a target")))?;
                let text = std::str::from_utf8(text)
                    .map_err(|_| unsafe_archive(format!("`{path}`: link target is not UTF-8")))?;
                let parts =
                    link_parts(text).map_err(|why| unsafe_archive(format!("`{path}`: {why}")))?;
                self.nodes.insert(path.clone(), Node::Symlink(parts));
                self.symlinks.push((path, text.to_owned()));
            }
            EntryKind::HardLink => {
                let target = entry
                    .link
                    .as_deref()
                    .ok_or_else(|| unsafe_archive(format!("`{path}`: link without a target")))?;
                let target = RelPath::from_bytes(target)
                    .ok()
                    .and_then(|t| t.strip(usize::from(self.artifact.strip)))
                    .filter(|t| matches!(self.nodes.get(t), Some(Node::File)))
                    .ok_or_else(|| {
                        unsafe_archive(format!("`{path}`: a hard link must name an earlier file"))
                    })?;
                self.nodes.insert(path.clone(), Node::HardLink);
                self.hard_links.push((path, target));
            }
            EntryKind::Other => {
                return Err(unsafe_archive(format!(
                    "`{path}` is a device or other special file"
                )));
            }
        }
        Ok(())
    }

    /// No parent may be a link or a file. Missing parents become implied directories.
    fn check_parents(&mut self, path: &RelPath) -> Result<(), InstallError> {
        for parent in path.ancestors() {
            match self.nodes.get(&parent) {
                None => {
                    self.nodes.insert(parent, Node::Dir);
                }
                Some(Node::Dir) => {}
                Some(_) => {
                    return Err(unsafe_archive(format!(
                        "`{path}` is under `{parent}`, which is not a directory"
                    )));
                }
            }
        }
        Ok(())
    }

    fn write_file(
        &mut self,
        path: &RelPath,
        mode: u32,
        data: &mut dyn Read,
    ) -> Result<(), InstallError> {
        // Permission bits only: setuid, setgid and sticky are cleared.
        let mode = match mode & 0o777 {
            0 => 0o644,
            m => m,
        };
        let mut file = self
            .fs
            .create_file_beneath(self.out, path, mode)
            .map_err(io)?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(InstallError::Cancelled);
            }
            let n = data
                .read(&mut buf)
                .map_err(|e| unsafe_archive(format!("`{path}`: {e}")))?;
            if n == 0 {
                break;
            }
            self.written += n as u64;
            if self.written > self.artifact.unpacked_size {
                return Err(unsafe_archive(
                    "more unpacked bytes than the catalog allows",
                ));
            }
            file.write_all(&buf[..n])
                .map_err(|e| InstallError::Io(e.to_string()))?;
        }
        self.fs.sync_file(&file).map_err(io)
    }

    /// Resolves and creates every link, symbolic first.
    fn links(&mut self) -> Result<(), InstallError> {
        for (path, text) in &self.symlinks {
            self.resolve(path)?;
            self.fs.symlink_beneath(self.out, path, text).map_err(io)?;
        }
        for (path, target) in &self.hard_links {
            self.fs
                .hard_link_beneath(self.out, path, target)
                .map_err(io)?;
        }
        Ok(())
    }

    /// Follows `link` through the archive's own links; every step must stay inside
    /// the output and the end must be an entry of the archive.
    fn resolve(&self, link: &RelPath) -> Result<RelPath, InstallError> {
        let escape = || unsafe_archive(format!("link `{link}` points outside the add-on"));
        let mut stack: Vec<String> = link.components().to_vec();
        stack.pop();
        let Some(Node::Symlink(first)) = self.nodes.get(link) else {
            return Err(escape());
        };
        let mut queue: VecDeque<String> = first.iter().cloned().collect();
        let mut hops = 1;
        while let Some(part) = queue.pop_front() {
            match part.as_str() {
                "." => continue,
                ".." => {
                    stack.pop().ok_or_else(escape)?;
                    continue;
                }
                _ => stack.push(part),
            }
            let here = RelPath::from_checked(stack.clone());
            if let Some(Node::Symlink(next)) = self.nodes.get(&here) {
                hops += 1;
                if hops > MAX_LINK_HOPS {
                    return Err(unsafe_archive(format!(
                        "link `{link}` follows too many links"
                    )));
                }
                stack.pop();
                for p in next.iter().rev() {
                    queue.push_front(p.clone());
                }
            }
        }
        let end = RelPath::from_checked(stack);
        if end.components().is_empty() || !self.nodes.contains_key(&end) {
            return Err(unsafe_archive(format!(
                "link `{link}` points to nothing in the archive"
            )));
        }
        Ok(end)
    }

    /// `library_dir` and `resources_dir` must be real directories; every `required`
    /// file ends up in `library_dir`.
    fn place_required(&self) -> Result<(), InstallError> {
        let lib = &self.artifact.library_dir;
        let res = &self.artifact.resources_dir;
        for dir in [lib, res] {
            if !matches!(self.nodes.get(dir), Some(Node::Dir)) || !self.real_dirs_above(dir) {
                return Err(unsafe_archive(format!(
                    "`{dir}` is not a directory in the archive"
                )));
            }
        }
        for name in &self.artifact.required {
            let in_lib = lib.join(name);
            if self.nodes.contains_key(&in_lib) {
                continue;
            }
            let in_res = res.join(name);
            let movable = matches!(self.nodes.get(&in_res), Some(Node::File | Node::Dir))
                && self.real_dirs_above(&in_res)
                && self.real_dirs_above(&in_lib);
            if !movable {
                return Err(unsafe_archive(format!("`{name}` is missing from `{lib}`")));
            }
            self.fs
                .rename(&in_res.under(self.out), &in_lib.under(self.out))
                .map_err(io)?;
        }
        Ok(())
    }

    /// Every ancestor of `path` is a directory of the archive (no link on the way).
    fn real_dirs_above(&self, path: &RelPath) -> bool {
        path.ancestors()
            .all(|a| matches!(self.nodes.get(&a), Some(Node::Dir)))
    }
}

/// The components of a symbolic link's text: relative, `.` and `..` allowed, every
/// other component passes the name rules.
fn link_parts(text: &str) -> Result<Vec<String>, String> {
    if text.is_empty() || text.starts_with('/') {
        return Err("a link target must be a non-empty relative path".into());
    }
    text.split('/')
        .map(|p| match p {
            "." | ".." => Ok(p.to_owned()),
            other => check_component(other).map(|()| other.to_owned()),
        })
        .collect()
}
