//! Installed add-ons on disk: install, clean, remove and resolve ([add-ons spec]).
//!
//! Layout under the root the composition root passes in:
//!
//! ```text
//! <root>/<id>/.lock              OS advisory lock for install, clean and remove
//! <root>/<id>/.staging/          download and unpack area, emptied before each attempt
//! <root>/<id>/<version>/         an installed version
//! <root>/<id>/<version>/.complete  written last; without it the directory is never used
//! <root>/<id>/.clean-pending     an update's clean still to run (written before the install)
//! ```
//!
//! [add-ons spec]: https://github.com/salihcantekin/rbs/blob/main/docs/specs/add-ons.md

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rbs_core::time::{Clock, Timestamp};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::archive::{Unpack, extract};
use crate::catalog::{Artifact, Entry, https_host};
use crate::fetch::{Fetch, FetchError, Response};
use crate::fs::{AddOnFs, FsError};
use crate::names::RelPath;
use crate::status::{InstallError, Status};
use crate::version::Version;

/// Name of the marker written last into an installed version.
pub const MARKER: &str = ".complete";
const LOCK: &str = ".lock";
const CLEAN_PENDING: &str = ".clean-pending";
const STAGING: &str = ".staging";
const DOWNLOAD: &str = "download";
const OUT: &str = "out";
/// Redirects followed per download.
const MAX_REDIRECTS: usize = 5;
/// Rename attempts while Windows reports a sharing violation.
const RENAME_ATTEMPTS: u32 = 10;
/// Pause between those attempts.
const RENAME_PAUSE: Duration = Duration::from_millis(200);
/// Shortest time between two progress updates of the same state.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
const CHUNK: usize = 64 * 1024;

/// An installed version a consumer can load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The version.
    pub version: String,
    /// Its directory.
    pub path: PathBuf,
    /// The directory that holds the library.
    pub library_dir: PathBuf,
    /// The directory that holds the other resources.
    pub resources_dir: PathBuf,
}

/// The marker's content.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    version: String,
    sha256: String,
    library_dir: String,
    resources_dir: String,
}

/// What an install reports to and reads from its caller.
pub struct InstallControl<'a> {
    /// Set by another thread to cancel; checked between reads and between entries.
    pub cancel: &'a AtomicBool,
    /// Throttles progress updates (ADR-0032: a `TestClock` in tests).
    pub clock: &'a dyn Clock,
    /// Receives `Downloading`, `Verifying` and `Installing`.
    pub progress: &'a mut dyn FnMut(Status),
}

/// The add-ons directory.
#[derive(Clone)]
pub struct AddOns {
    root: PathBuf,
    fs: Arc<dyn AddOnFs>,
}

impl std::fmt::Debug for AddOns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddOns")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

fn fs_err(e: FsError) -> InstallError {
    match e {
        FsError::Busy => InstallError::Busy,
        other => InstallError::Io(other.to_string()),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

impl AddOns {
    /// The add-ons under `root`. Nothing is read or written until a call asks.
    pub fn new(root: PathBuf, fs: Arc<dyn AddOnFs>) -> Self {
        Self { root, fs }
    }

    fn dir(&self, entry: &Entry) -> PathBuf {
        self.root.join(entry.id.as_str())
    }

    /// The catalog's version when it is installed with its marker.
    pub fn installed(&self, entry: &Entry) -> Option<Installed> {
        let path = self.dir(entry).join(entry.version.as_str());
        let marker = self.read_marker(&path)?;
        (Version::parse(&marker.version).as_ref() == Some(&entry.version)).then_some(())?;
        let lib = RelPath::parse(&marker.library_dir).ok()?;
        let res = RelPath::parse(&marker.resources_dir).ok()?;
        Some(Installed {
            version: entry.version.as_str().to_owned(),
            library_dir: lib.under(&path),
            resources_dir: res.under(&path),
            path,
        })
    }

    fn read_marker(&self, version_dir: &Path) -> Option<Marker> {
        let mut file = self.fs.open_read(&version_dir.join(MARKER)).ok()?;
        let mut text = String::new();
        Read::by_ref(&mut file)
            .take(4096)
            .read_to_string(&mut text)
            .ok()?;
        toml::from_str(&text).ok()
    }

    /// The status while no install of `entry` runs.
    pub fn status(&self, entry: &Entry) -> Status {
        if let Some(i) = self.installed(entry) {
            return Status::Installed { version: i.version };
        }
        let lower = self
            .marked_versions(entry)
            .into_iter()
            .filter(|v| *v < entry.version)
            .max();
        match lower {
            Some(v) => Status::UpdateAvailable {
                installed: v.as_str().to_owned(),
                available: entry.version.as_str().to_owned(),
            },
            None => Status::NotInstalled,
        }
    }

    /// Versions on disk whose marker names that version.
    fn marked_versions(&self, entry: &Entry) -> Vec<Version> {
        let dir = self.dir(entry);
        self.fs
            .list_dir(&dir)
            .unwrap_or_default()
            .iter()
            .filter_map(|name| Version::parse(name))
            .filter(|v| {
                self.read_marker(&dir.join(v.as_str()))
                    .is_some_and(|m| m.version == v.as_str())
            })
            .collect()
    }

    /// Downloads, verifies and installs the catalog's version of `entry`.
    ///
    /// Runs on a thread the caller made for this install. Never removes or replaces
    /// a version with a marker.
    pub fn install(
        &self,
        entry: &Entry,
        fetch: &dyn Fetch,
        unpack: &dyn Unpack,
        control: InstallControl<'_>,
    ) -> Result<Installed, InstallError> {
        let artifact = entry.artifact.as_ref().ok_or(InstallError::NotAvailable)?;
        let dir = self.dir(entry);
        let root_rel = RelPath::single(entry.id.as_str()).map_err(InstallError::Io)?;
        self.fs
            .create_dir_beneath(&self.root, &root_rel)
            .map_err(fs_err)?;
        let _lock = self.fs.lock(&dir.join(LOCK)).map_err(fs_err)?;
        if let Some(done) = self.installed(entry) {
            return Ok(done);
        }
        let staging = dir.join(STAGING);
        let result = self.install_locked(entry, artifact, fetch, unpack, control, &dir, &staging);
        // Staging is emptied after success and failure alike; a failure here only
        // leaves files that the next attempt deletes first.
        let _ = self.fs.remove_all(&staging);
        result
    }

    #[allow(clippy::too_many_arguments)] // One call site; the steps read in spec order.
    fn install_locked(
        &self,
        entry: &Entry,
        artifact: &Artifact,
        fetch: &dyn Fetch,
        unpack: &dyn Unpack,
        control: InstallControl<'_>,
        dir: &Path,
        staging: &Path,
    ) -> Result<Installed, InstallError> {
        let InstallControl {
            cancel,
            clock,
            progress,
        } = control;
        // Step 2: empty staging.
        self.fs.remove_all(staging).map_err(fs_err)?;
        let staging_rel = RelPath::single(STAGING).map_err(InstallError::Io)?;
        self.fs
            .create_dir_beneath(dir, &staging_rel)
            .map_err(fs_err)?;
        // Step 3: free space.
        let need = artifact.size.saturating_add(artifact.unpacked_size);
        if self.fs.free_space(dir).map_err(fs_err)? < need {
            return Err(InstallError::NoSpace);
        }
        // Step 4: download.
        let download_rel = RelPath::single(DOWNLOAD).map_err(InstallError::Io)?;
        let mut file = self
            .fs
            .create_file_beneath(staging, &download_rel, 0o600)
            .map_err(fs_err)?;
        let mut body = open(fetch, &artifact.url, &entry.hosts)?;
        download(
            &mut *body,
            &mut file,
            artifact.size,
            cancel,
            clock,
            progress,
        )?;
        self.fs.sync_file(&file).map_err(fs_err)?;
        // Step 5: hash the same open file.
        progress(Status::Verifying);
        file.seek(SeekFrom::Start(0))
            .map_err(|e| InstallError::Io(e.to_string()))?;
        if sha256(&mut file, cancel)? != artifact.sha256 {
            return Err(InstallError::HashMismatch);
        }
        // Steps 6 and 7: unpack and links.
        progress(Status::Installing);
        file.seek(SeekFrom::Start(0))
            .map_err(|e| InstallError::Io(e.to_string()))?;
        let out_rel = RelPath::single(OUT).map_err(InstallError::Io)?;
        self.fs
            .create_dir_beneath(staging, &out_rel)
            .map_err(fs_err)?;
        let out = staging.join(OUT);
        extract(self.fs.as_ref(), unpack, artifact, &mut file, &out, cancel)?;
        drop(file);
        // Step 8: marker, rename, sync.
        let marker = Marker {
            version: entry.version.as_str().to_owned(),
            sha256: hex(&artifact.sha256),
            library_dir: artifact.library_dir.to_string(),
            resources_dir: artifact.resources_dir.to_string(),
        };
        let text = toml::to_string(&marker).map_err(|e| InstallError::Io(e.to_string()))?;
        let marker_rel = RelPath::single(MARKER).map_err(InstallError::Io)?;
        let mut m = self
            .fs
            .create_file_beneath(&out, &marker_rel, 0o644)
            .map_err(fs_err)?;
        m.write_all(text.as_bytes())
            .map_err(|e| InstallError::Io(e.to_string()))?;
        self.fs.sync_file(&m).map_err(fs_err)?;
        drop(m);
        self.fs.sync_dir(&out).map_err(fs_err)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(InstallError::Cancelled);
        }
        let target = dir.join(entry.version.as_str());
        if self.fs.exists(&target) {
            // No marker for this version (checked under the lock): an unfinished
            // directory that is never used.
            self.fs.remove_all(&target).map_err(fs_err)?;
        }
        self.rename_with_retry(&out, &target)?;
        self.fs.sync_dir(dir).map_err(fs_err)?;
        self.installed(entry)
            .ok_or_else(|| InstallError::Io("the installed version cannot be read back".into()))
    }

    fn rename_with_retry(&self, from: &Path, to: &Path) -> Result<(), InstallError> {
        let mut attempt = 1;
        loop {
            match self.fs.rename(from, to) {
                Ok(()) => return Ok(()),
                Err(FsError::SharingViolation) if attempt < RENAME_ATTEMPTS => {
                    attempt += 1;
                    self.fs.pause(RENAME_PAUSE);
                }
                Err(e) => return Err(InstallError::Io(e.to_string())),
            }
        }
    }

    /// Removes versions lower than the highest version below the catalog's. Keeps
    /// the catalog's version, the previous one, every higher one and every directory
    /// whose name is not a version.
    pub fn clean(&self, entry: &Entry) -> Result<(), InstallError> {
        let dir = self.dir(entry);
        if !self.fs.exists(&dir) {
            return Ok(());
        }
        let _lock = self.fs.lock(&dir.join(LOCK)).map_err(fs_err)?;
        let previous = self
            .marked_versions(entry)
            .into_iter()
            .filter(|v| *v < entry.version)
            .max();
        let Some(previous) = previous else {
            return Ok(());
        };
        let doomed: Vec<Version> = self
            .fs
            .list_dir(&dir)
            .map_err(fs_err)?
            .iter()
            .filter_map(|n| Version::parse(n))
            .filter(|v| *v < previous)
            .collect();
        self.delete_versions(&dir, doomed.iter().map(Version::as_str))
    }

    /// Notes, before an update's install starts, that its clean is still to run: the
    /// note outlives a quit between the install and the clean (add-ons spec, "Update").
    pub fn mark_clean_pending(&self, entry: &Entry) -> Result<(), InstallError> {
        let dir = self.dir(entry);
        let root_rel = RelPath::single(entry.id.as_str()).map_err(InstallError::Io)?;
        self.fs
            .create_dir_beneath(&self.root, &root_rel)
            .map_err(fs_err)?;
        if self.fs.exists(&dir.join(CLEAN_PENDING)) {
            return Ok(());
        }
        let rel = RelPath::single(CLEAN_PENDING).map_err(InstallError::Io)?;
        let file = self
            .fs
            .create_file_beneath(&dir, &rel, 0o644)
            .map_err(fs_err)?;
        self.fs.sync_file(&file).map_err(fs_err)?;
        self.fs.sync_dir(&dir).map_err(fs_err)
    }

    /// True while an update's clean is still to run (one check of the disk).
    pub fn clean_pending(&self, entry: &Entry) -> bool {
        self.fs.exists(&self.dir(entry).join(CLEAN_PENDING))
    }

    /// Ends an update: when the catalog's version is installed, cleans as [`clean`]
    /// does; either way the note goes (an install that did not finish keeps the old
    /// version, and nothing is left to do). True when it cleaned.
    ///
    /// [`clean`]: AddOns::clean
    pub fn finish_update(&self, entry: &Entry) -> Result<bool, InstallError> {
        let note = self.dir(entry).join(CLEAN_PENDING);
        if !self.fs.exists(&note) {
            return Ok(false);
        }
        let cleaned = self.installed(entry).is_some();
        if cleaned {
            self.clean(entry)?;
        }
        match self.fs.remove_file(&note) {
            Ok(()) | Err(FsError::NotFound) => Ok(cleaned),
            Err(e) => Err(fs_err(e)),
        }
    }

    /// Removes every version and the staging area of `entry`.
    pub fn remove(&self, entry: &Entry) -> Result<(), InstallError> {
        let dir = self.dir(entry);
        if !self.fs.exists(&dir) {
            return Ok(());
        }
        let _lock = self.fs.lock(&dir.join(LOCK)).map_err(fs_err)?;
        let names: Vec<String> = self
            .fs
            .list_dir(&dir)
            .map_err(fs_err)?
            .into_iter()
            .filter(|n| n != LOCK && n != CLEAN_PENDING)
            .collect();
        match self.fs.remove_file(&dir.join(CLEAN_PENDING)) {
            Ok(()) | Err(FsError::NotFound) => {}
            Err(e) => return Err(fs_err(e)),
        }
        self.delete_versions(&dir, names.iter().map(String::as_str))
    }

    /// Deletes each marker first, then the tree; tries every one and reports the
    /// first failure.
    fn delete_versions<'n>(
        &self,
        dir: &Path,
        names: impl Iterator<Item = &'n str>,
    ) -> Result<(), InstallError> {
        let mut first_error = None;
        for name in names {
            let path = dir.join(name);
            let marker = self.fs.remove_file(&path.join(MARKER));
            let tree = self.fs.remove_all(&path);
            let failed = match (marker, tree) {
                (Ok(()) | Err(FsError::NotFound), Ok(())) => None,
                (Err(e), _) | (_, Err(e)) => Some(e),
            };
            if let (Some(e), None) = (failed, &first_error) {
                first_error = Some(InstallError::Io(format!("{name}: {e}")));
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// Follows redirects, checking `https` and the host list at every hop.
fn open(
    fetch: &dyn Fetch,
    url: &str,
    hosts: &[String],
) -> Result<Box<dyn Read + Send>, InstallError> {
    let mut url = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        if !url.starts_with("https://") {
            return Err(InstallError::NotHttps);
        }
        let host = https_host(&url).map_err(InstallError::HostNotAllowed)?;
        if !hosts.contains(&host) {
            return Err(InstallError::HostNotAllowed(host));
        }
        match fetch.get(&url).map_err(fetch_err)? {
            Response::Body(body) => return Ok(body),
            Response::Redirect(next) => url = next,
        }
    }
    Err(InstallError::Network(format!(
        "more than {MAX_REDIRECTS} redirects"
    )))
}

fn fetch_err(e: FetchError) -> InstallError {
    match e {
        FetchError::Network(e) => InstallError::Network(e),
        FetchError::Stalled => InstallError::Stalled,
    }
}

fn read_err(e: std::io::Error) -> InstallError {
    let stalled = e.kind() == std::io::ErrorKind::TimedOut
        || e.get_ref()
            .and_then(|inner| inner.downcast_ref::<FetchError>())
            .is_some_and(|f| *f == FetchError::Stalled);
    if stalled {
        InstallError::Stalled
    } else {
        InstallError::Network(e.to_string())
    }
}

fn download(
    body: &mut dyn Read,
    file: &mut std::fs::File,
    size: u64,
    cancel: &AtomicBool,
    clock: &dyn Clock,
    progress: &mut dyn FnMut(Status),
) -> Result<(), InstallError> {
    let mut buf = vec![0u8; CHUNK];
    let mut bytes = 0u64;
    progress(Status::Downloading { bytes, total: size });
    let mut last: Timestamp = clock.now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(InstallError::Cancelled);
        }
        let n = body.read(&mut buf).map_err(read_err)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > size {
            return Err(InstallError::TooLarge);
        }
        file.write_all(&buf[..n])
            .map_err(|e| InstallError::Io(e.to_string()))?;
        let now = clock.now();
        if now.saturating_since(last) >= PROGRESS_EVERY {
            last = now;
            progress(Status::Downloading { bytes, total: size });
        }
    }
    if bytes < size {
        return Err(InstallError::Network(format!(
            "the download ended after {bytes} of {size} bytes"
        )));
    }
    progress(Status::Downloading { bytes, total: size });
    Ok(())
}

fn sha256(file: &mut std::fs::File, cancel: &AtomicBool) -> Result<[u8; 32], InstallError> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(InstallError::Cancelled);
        }
        let n = file
            .read(&mut buf)
            .map_err(|e| InstallError::Io(e.to_string()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}
