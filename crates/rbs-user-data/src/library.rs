//! A library: a directory of named entries of one document kind, one file each.
//!
//! The file name is the entry's id ([`EntryId`]): chosen once from the name when the
//! entry is created and never changed, so a rename touches one line of one file and
//! anything that refers to the entry by id keeps working. The display name is inside
//! the file and unique in the library, compared ignoring case. Deleting moves the file
//! into `deleted/`, so a mistake can be undone by hand (and later by the app). Only
//! files ending in `.toml` directly in the directory are entries.

use std::fmt;
use std::fs;
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use crate::document::{self, DataError, Document};

/// Longest entry name, in characters.
pub const MAX_NAME: usize = 64;

/// Longest entry id, in bytes.
pub const MAX_ID: usize = 64;

/// Subdirectory deleted entries are moved to.
pub const DELETED_DIR: &str = "deleted";

const EXTENSION: &str = "toml";

/// Entry files by id, and `.toml` files whose name is not an id.
type Files = (Vec<(EntryId, PathBuf)>, Vec<PathBuf>);

/// An entry's id: its file name without `.toml`. Lowercase ASCII letters, digits and
/// `-`, 1 to [`MAX_ID`] bytes, not starting or ending with `-`, and not a name Windows
/// reserves for devices (`con`, `nul`, `com1`, ...). Never reused within a library,
/// also after a delete.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryId(String);

impl EntryId {
    /// The id, if `text` is a valid one.
    pub fn parse(text: &str) -> Option<Self> {
        let ok = !text.is_empty()
            && text.len() <= MAX_ID
            && !text.starts_with('-')
            && !text.ends_with('-')
            && text
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !is_device_name(text);
        ok.then(|| Self(text.to_owned()))
    }

    /// An id made from a name: letters lowercased (common accented Latin letters
    /// without their accent), digits kept, every other run of characters one `-`;
    /// `entry` if nothing is left.
    pub fn from_name(name: &str) -> Self {
        let mut s = String::new();
        for c in name.chars().map(plain) {
            if c.is_ascii_alphanumeric() {
                s.push(c.to_ascii_lowercase());
            } else if !s.is_empty() && !s.ends_with('-') {
                s.push('-');
            }
        }
        // Leave room for a "-NNN" suffix.
        s.truncate(MAX_ID - 8);
        let s = s.trim_end_matches('-');
        let s = if s.is_empty() { "entry" } else { s };
        if is_device_name(s) {
            Self(format!("{s}-1"))
        } else {
            Self(s.to_owned())
        }
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Names Windows reserves for devices, with or without an extension.
fn is_device_name(id: &str) -> bool {
    matches!(id, "con" | "prn" | "aux" | "nul")
        || ((id.starts_with("com") || id.starts_with("lpt"))
            && id.len() == 4
            && id.as_bytes()[3].is_ascii_digit())
}

/// `c` without its accent, for the common accented Latin letters; others unchanged.
fn plain(c: char) -> char {
    const FROM: &str = "ÀÁÂÃÄÅàáâãäåÇçĆćČčÈÉÊËèéêëĞğÌÍÎÏìíîïİıÑñÒÓÔÕÖØòóôõöøŞşŠšÙÚÛÜùúûüÝýÿŽž";
    const TO: &str = "AAAAAAaaaaaaCcCcCcEEEEeeeeGgIIIIiiiiIiNnOOOOOOooooooSsSsUUUUuuuuYyyZz";
    FROM.chars()
        .position(|f| f == c)
        .and_then(|i| TO.chars().nth(i))
        .unwrap_or(c)
}

/// A name as it will be stored: trimmed, checked.
pub fn check_name(name: &str) -> Result<String, DataError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(DataError::BadName("a name cannot be empty".to_owned()));
    }
    if name.chars().count() > MAX_NAME {
        return Err(DataError::BadName(format!(
            "a name can have at most {MAX_NAME} characters"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(DataError::BadName(
            "a name cannot have line breaks or control characters".to_owned(),
        ));
    }
    // Direction overrides make a shared name look like another one.
    if name.chars().any(|c| {
        matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
    }) {
        return Err(DataError::BadName(
            "a name cannot have text direction marks".to_owned(),
        ));
    }
    Ok(name.to_owned())
}

fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// An entry: its name and contents.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Entry<T> {
    /// Display name, unique in the library.
    pub name: String,
    /// Contents.
    pub document: T,
}

impl<T> Entry<T> {
    /// An entry named `name`.
    pub fn new(name: impl Into<String>, document: T) -> Self {
        Self {
            name: name.into(),
            document,
        }
    }
}

/// One entry of [`Library::list`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// Its id.
    pub id: EntryId,
    /// Its name.
    pub name: String,
}

/// What [`Library::list`] found.
#[non_exhaustive]
#[derive(Debug, Default)]
pub struct Listing {
    /// Readable entries, by name ignoring case.
    pub entries: Vec<Listed>,
    /// Files that could not be read (including ones made by a newer RBS, and `.toml`
    /// files whose name is not an id), with the reason. They are left untouched and
    /// their ids stay taken.
    pub problems: Vec<(PathBuf, DataError)>,
}

/// The entries of one document kind in one directory.
#[derive(Debug, Clone)]
pub struct Library<T> {
    dir: PathBuf,
    kind: PhantomData<fn() -> T>,
}

impl<T: Document> Library<T> {
    /// A library in `dir`; the directory is created by the first write.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            kind: PhantomData,
        }
    }

    /// Its directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file of an entry.
    pub fn path(&self, id: &EntryId) -> PathBuf {
        self.dir.join(format!("{}.{EXTENSION}", id.as_str()))
    }

    /// Ids of every entry file, readable or not, sorted, and `.toml` files whose name
    /// is not an id.
    fn files(&self) -> Result<Files, DataError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
            Err(e) => return Err(e.into()),
        };
        let mut ids = Vec::new();
        let mut odd = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_file()
                || path.extension().and_then(|e| e.to_str()) != Some(EXTENSION)
            {
                continue;
            }
            match path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(EntryId::parse)
            {
                Some(id) => ids.push((id, path)),
                None => odd.push(path),
            }
        }
        ids.sort();
        odd.sort();
        Ok((ids, odd))
    }

    /// Every id ever used here: live entries, their backups, and deleted
    /// entries (`deleted/<id>.toml` and `deleted/<id>-N.toml`), so an id is never given
    /// to a second entry. Credential store entries and other references by id can
    /// therefore never reach a different entry.
    fn used_ids(&self) -> Result<Vec<String>, DataError> {
        let mut used: Vec<String> = Vec::new();
        // Every file here, including backups (`<id>.toml.format1.bak`,
        // `<id>.toml.unreadable.bak`): the part of its name before the first dot.
        match fs::read_dir(&self.dir) {
            Ok(entries) => {
                for entry in entries {
                    let name = entry?.file_name();
                    if let Some(stem) = name.to_str().and_then(|n| n.split('.').next()) {
                        used.push(stem.to_owned());
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        match fs::read_dir(self.dir.join(DELETED_DIR)) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry?.path();
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        used.push(stem.to_owned());
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(used)
    }

    /// Every entry, readable ones by name, the rest as problems. Reads only each
    /// file's envelope (format, kind, name); a body that does not load is reported by
    /// [`Library::load`].
    pub fn list(&self) -> Result<Listing, DataError> {
        let mut listing = Listing::default();
        let (ids, odd) = self.files()?;
        for (id, path) in ids {
            let named = document::peek::<T>(&path).and_then(|e| {
                let e = e.ok_or_else(|| DataError::NotFound(id.to_string()))?;
                let name = e
                    .name
                    .ok_or_else(|| DataError::Invalid("the entry has no name".to_owned()))?;
                check_name(&name)
            });
            match named {
                Ok(name) => listing.entries.push(Listed { id, name }),
                Err(e) => listing.problems.push((path, e)),
            }
        }
        for path in odd {
            listing.problems.push((
                path,
                DataError::BadName(
                    "the file name is not an entry id (lowercase letters, digits and -)".to_owned(),
                ),
            ));
        }
        listing
            .entries
            .sort_by_key(|e| (e.name.to_lowercase(), e.id.clone()));
        Ok(listing)
    }

    fn read(&self, path: &Path) -> Result<Entry<T>, DataError> {
        let read = document::read::<T>(path)?
            .ok_or_else(|| DataError::NotFound(path.display().to_string()))?;
        let name = read
            .name
            .ok_or_else(|| DataError::Invalid("the entry has no name".to_owned()))?;
        Ok(Entry {
            name: check_name(&name)?,
            document: read.document,
        })
    }

    /// The entry with `id`.
    pub fn load(&self, id: &EntryId) -> Result<Entry<T>, DataError> {
        let path = self.path(id);
        if !path.is_file() {
            return Err(DataError::NotFound(id.to_string()));
        }
        self.read(&path)
    }

    /// The id of the entry named `name` (ignoring case), if any.
    pub fn find(&self, name: &str) -> Result<Option<EntryId>, DataError> {
        Ok(self
            .list()?
            .entries
            .into_iter()
            .find(|e| same_name(&e.name, name.trim()))
            .map(|e| e.id))
    }

    fn check_free(&self, name: &str, except: Option<&EntryId>) -> Result<(), DataError> {
        let taken = self
            .list()?
            .entries
            .iter()
            .any(|e| Some(&e.id) != except && same_name(&e.name, name));
        if taken {
            Err(DataError::NameTaken(name.to_owned()))
        } else {
            Ok(())
        }
    }

    /// An id made from `name` that was never used here.
    fn new_id(&self, name: &str) -> Result<EntryId, DataError> {
        let base = EntryId::from_name(name);
        let used = self.used_ids()?;
        let free = |id: &EntryId| !used.contains(&id.0) && !self.path(id).exists();
        if free(&base) {
            return Ok(base);
        }
        (2..u32::MAX)
            .map(|n| EntryId(format!("{}-{n}", base.as_str())))
            .find(free)
            .ok_or_else(|| DataError::Invalid("no free id".to_owned()))
    }

    /// Adds an entry named `name`; its id is made from the name.
    pub fn create(&self, name: &str, document: &T) -> Result<EntryId, DataError> {
        let name = check_name(name)?;
        self.check_free(&name, None)?;
        let id = self.new_id(&name)?;
        document::write(&self.path(&id), document, Some(&name))?;
        Ok(id)
    }

    /// Replaces the contents and name of an existing entry.
    pub fn save(&self, id: &EntryId, entry: &Entry<T>) -> Result<(), DataError> {
        let name = check_name(&entry.name)?;
        let path = self.path(id);
        if !path.is_file() {
            return Err(DataError::NotFound(id.to_string()));
        }
        // Most saves keep the name (an autosave after an edit): no need to read every
        // other entry then.
        let same = document::peek::<T>(&path)
            .ok()
            .flatten()
            .and_then(|e| e.name)
            .is_some_and(|old| old.trim() == name);
        if !same {
            self.check_free(&name, Some(id))?;
        }
        document::write(&path, &entry.document, Some(&name))
    }

    /// Renames an entry; its id stays.
    pub fn rename(&self, id: &EntryId, name: &str) -> Result<(), DataError> {
        let mut entry = self.load(id)?;
        entry.name = name.to_owned();
        self.save(id, &entry)
    }

    /// Copies an entry under a new name.
    pub fn duplicate(&self, id: &EntryId, name: &str) -> Result<EntryId, DataError> {
        let entry = self.load(id)?;
        self.create(name, &entry.document)
    }

    /// Moves an entry's file to the `deleted` subdirectory (as `<id>.toml`, or
    /// `<id>-N.toml` if that is taken) and returns where it went.
    pub fn delete(&self, id: &EntryId) -> Result<PathBuf, DataError> {
        let path = self.path(id);
        if !path.is_file() {
            return Err(DataError::NotFound(id.to_string()));
        }
        let bin = self.dir.join(DELETED_DIR);
        fs::create_dir_all(&bin)?;
        let mut to = bin.join(format!("{}.{EXTENSION}", id.as_str()));
        let mut n = 2u32;
        while to.exists() {
            to = bin.join(format!("{}-{n}.{EXTENSION}", id.as_str()));
            n = n.saturating_add(1);
        }
        fs::rename(&path, &to)?;
        Ok(to)
    }

    /// Writes an entry to a file outside the library, in the current format, for
    /// sharing. Same as [`Library::export_with`] with nothing removed.
    pub fn export(&self, id: &EntryId, to: &Path) -> Result<(), DataError> {
        self.export_with(id, to, |_| {})
    }

    /// Writes an entry to a file outside the library, after `scrub` has removed what
    /// belongs to this computer only (folders, "a key is saved" flags; ADR-0051).
    /// Documents never hold secrets. A target in the library's own directory is
    /// refused (that would bypass the name rules); an existing target is replaced only
    /// under the rules of [`document::write`].
    pub fn export_with(
        &self,
        id: &EntryId,
        to: &Path,
        scrub: impl FnOnce(&mut T),
    ) -> Result<(), DataError> {
        let mut entry = self.load(id)?;
        scrub(&mut entry.document);
        let parent = to.parent().filter(|p| !p.as_os_str().is_empty());
        let inside = match (parent.map(fs::canonicalize), fs::canonicalize(&self.dir)) {
            (Some(Ok(p)), Ok(d)) => p.starts_with(&d),
            _ => false,
        };
        if inside {
            return Err(DataError::Invalid(
                "export to a file outside the library folder".to_owned(),
            ));
        }
        document::write(to, &entry.document, Some(&entry.name))
    }

    /// Adds the entry in a file made by [`Library::export`] (or by hand). Older formats
    /// are upgraded; newer ones and other kinds are refused. A file without a name is
    /// named after the file; a name already used gets a number (`Name 2`). Returns the
    /// new id and the name it got.
    pub fn import(&self, from: &Path) -> Result<(EntryId, String), DataError> {
        let read = document::read::<T>(from)?
            .ok_or_else(|| DataError::NotFound(from.display().to_string()))?;
        let wanted = read
            .name
            .or_else(|| from.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let wanted: String = wanted.trim().chars().take(MAX_NAME).collect();
        let wanted = check_name(&wanted).or_else(|_| check_name("Imported"))?;
        let names: Vec<String> = self.list()?.entries.into_iter().map(|e| e.name).collect();
        let free = |n: &str| !names.iter().any(|m| same_name(m, n));
        let name = if free(&wanted) {
            wanted
        } else {
            (2..u32::MAX)
                .map(|n| {
                    let suffix = format!(" {n}");
                    let keep = MAX_NAME - suffix.chars().count();
                    let head: String = wanted.chars().take(keep).collect();
                    format!("{}{suffix}", head.trim_end())
                })
                .find(|n| free(n))
                .ok_or_else(|| DataError::Invalid("no free name".to_owned()))?
        };
        let id = self.create(&name, &read.document)?;
        Ok((id, name))
    }
}
