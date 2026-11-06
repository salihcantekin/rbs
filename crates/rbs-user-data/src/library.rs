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

/// An entry's id: its file name without `.toml`. Lowercase ASCII letters, digits and
/// `-`, 1 to [`MAX_ID`] bytes, not starting or ending with `-`.
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
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
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
        Self(if s.is_empty() { "entry" } else { s }.to_owned())
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
    Ok(name.to_owned())
}

fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// An entry: its name and contents.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry<T> {
    /// Display name, unique in the library.
    pub name: String,
    /// Contents.
    pub document: T,
}

/// One entry of [`Library::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// Its id.
    pub id: EntryId,
    /// Its name.
    pub name: String,
}

/// What [`Library::list`] found.
#[derive(Debug, Default)]
pub struct Listing {
    /// Readable entries, by name ignoring case.
    pub entries: Vec<Listed>,
    /// Files that could not be read (including ones made by a newer RBS), with the
    /// reason. They are left untouched and their ids stay taken.
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

    /// Ids of every entry file, readable or not, sorted.
    fn ids(&self) -> Result<Vec<(EntryId, PathBuf)>, DataError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_file()
                || path.extension().and_then(|e| e.to_str()) != Some(EXTENSION)
            {
                continue;
            }
            if let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(EntryId::parse)
            {
                ids.push((id, path));
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Every entry, readable ones by name, the rest as problems.
    pub fn list(&self) -> Result<Listing, DataError> {
        let mut listing = Listing::default();
        for (id, path) in self.ids()? {
            match self.read(&path) {
                Ok(e) => listing.entries.push(Listed { id, name: e.name }),
                Err(e) => listing.problems.push((path, e)),
            }
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

    /// An unused id made from `name`.
    fn new_id(&self, name: &str) -> Result<EntryId, DataError> {
        let base = EntryId::from_name(name);
        let used: Vec<EntryId> = self.ids()?.into_iter().map(|(id, _)| id).collect();
        let free = |id: &EntryId| !used.contains(id) && !self.path(id).exists();
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
        self.check_free(&name, Some(id))?;
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
    /// sharing. Documents never hold secrets (ADR-0051), so the file can be given away.
    pub fn export(&self, id: &EntryId, to: &Path) -> Result<(), DataError> {
        let entry = self.load(id)?;
        document::write_atomic(
            to,
            document::render(&entry.document, Some(&entry.name))?.as_bytes(),
        )
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
