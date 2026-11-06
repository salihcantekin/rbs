//! Versioned documents: one TOML file each, with `format` and `kind` at the top.
//!
//! Reading checks the kind, refuses a newer format and upgrades an older one in memory,
//! one step at a time. Writing replaces the file in one step (a temporary file renamed
//! over it), never overwrites a file of a newer format, and keeps a copy of a file of an
//! older format the first time it is rewritten.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Key of the format version in every document.
pub const FORMAT_KEY: &str = "format";
/// Key of the document kind in every document.
pub const KIND_KEY: &str = "kind";
/// Key of the display name in library entries.
pub const NAME_KEY: &str = "name";

/// A kind of document: how its body maps to and from TOML, and how older formats are
/// upgraded.
pub trait Document: Sized {
    /// The `kind` written into every file, for example `"scene-collection"`. Reading a
    /// file of another kind fails.
    const KIND: &'static str;
    /// The format version this build writes. Files with a higher version are refused;
    /// files with a lower one are upgraded with [`Document::upgrade`].
    const FORMAT: u32;

    /// Upgrades `body` (the file without `format`, `kind` and `name`) from format `from`
    /// to `from + 1`. The default refuses: a kind starts at format 1 with nothing to
    /// upgrade.
    fn upgrade(from: u32, body: &mut toml::Table) -> Result<(), String> {
        let _ = body;
        Err(format!("format {from} cannot be read"))
    }

    /// The document from its body in the current format.
    fn from_body(body: toml::Table) -> Result<Self, String>;

    /// The body in the current format. Must not use the keys `format`, `kind` or `name`.
    fn to_body(&self) -> Result<toml::Table, String>;
}

/// A body from a serde type (helper for [`Document::to_body`]).
pub fn body_of<T: Serialize>(value: &T) -> Result<toml::Table, String> {
    match toml::Value::try_from(value).map_err(|e| e.to_string())? {
        toml::Value::Table(t) => Ok(t),
        _ => Err("a document body must be a table".to_owned()),
    }
}

/// A serde type from a body (helper for [`Document::from_body`]).
pub fn from_body<T: DeserializeOwned>(body: toml::Table) -> Result<T, String> {
    toml::Value::Table(body)
        .try_into()
        .map_err(|e: toml::de::Error| e.message().to_owned())
}

/// Why user data could not be read or written.
#[non_exhaustive]
#[derive(Debug)]
pub enum DataError {
    /// Reading or writing failed.
    Io(io::Error),
    /// The file is not valid TOML.
    Syntax(String),
    /// The file has no `format` number, or no `kind`.
    NoVersion,
    /// The file holds another kind of document.
    WrongKind {
        /// Its kind.
        found: String,
        /// The kind asked for.
        expected: &'static str,
    },
    /// The file was written by a newer RBS. It is never overwritten.
    Newer {
        /// Its format.
        found: u32,
        /// The newest format this build reads.
        supported: u32,
    },
    /// The document's contents are not valid (the reason in words).
    Invalid(String),
    /// A name is empty, too long or has control characters.
    BadName(String),
    /// Another entry already has this name (compared ignoring case).
    NameTaken(String),
    /// No entry with this id.
    NotFound(String),
}

impl fmt::Display for DataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataError::Io(e) => e.fmt(f),
            DataError::Syntax(e) => write!(f, "not a valid settings file: {e}"),
            DataError::NoVersion => f.write_str("not an RBS file: no format version or kind"),
            DataError::WrongKind { found, expected } => {
                write!(f, "holds a {found}, not a {expected}")
            }
            DataError::Newer { found, supported } => write!(
                f,
                "made by a newer RBS (format {found}; this version reads up to {supported})"
            ),
            DataError::Invalid(e) => e.fmt(f),
            DataError::BadName(why) => why.fmt(f),
            DataError::NameTaken(n) => write!(f, "the name {n:?} is already used"),
            DataError::NotFound(id) => write!(f, "{id:?} does not exist"),
        }
    }
}

impl std::error::Error for DataError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DataError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for DataError {
    fn from(e: io::Error) -> Self {
        DataError::Io(e)
    }
}

/// A document as read: its display name (library entries have one) and contents.
#[derive(Debug, Clone, PartialEq)]
pub struct Read<T> {
    /// The `name` in the file, if any.
    pub name: Option<String>,
    /// The format the file had before upgrading (equal to `T::FORMAT` if none was
    /// needed).
    pub format: u32,
    /// The contents, in the current format.
    pub document: T,
}

/// Parses a document from text: checks `kind`, refuses a newer `format`, upgrades an
/// older one.
pub fn parse<T: Document>(text: &str) -> Result<Read<T>, DataError> {
    let mut table: toml::Table =
        toml::from_str(text).map_err(|e: toml::de::Error| DataError::Syntax(e.to_string()))?;
    let format = match table.remove(FORMAT_KEY) {
        Some(toml::Value::Integer(v)) => u32::try_from(v).map_err(|_| DataError::NoVersion)?,
        _ => return Err(DataError::NoVersion),
    };
    match table.remove(KIND_KEY) {
        Some(toml::Value::String(k)) if k == T::KIND => {}
        Some(toml::Value::String(k)) => {
            return Err(DataError::WrongKind {
                found: k,
                expected: T::KIND,
            });
        }
        _ => return Err(DataError::NoVersion),
    }
    if format > T::FORMAT {
        return Err(DataError::Newer {
            found: format,
            supported: T::FORMAT,
        });
    }
    if format == 0 {
        return Err(DataError::NoVersion);
    }
    let name = match table.remove(NAME_KEY) {
        None => None,
        Some(toml::Value::String(n)) => Some(n),
        Some(_) => return Err(DataError::Invalid("name must be text".to_owned())),
    };
    for from in format..T::FORMAT {
        T::upgrade(from, &mut table).map_err(DataError::Invalid)?;
    }
    let document = T::from_body(table).map_err(DataError::Invalid)?;
    Ok(Read {
        name,
        format,
        document,
    })
}

/// The text of a document in the current format, with `name` if given.
pub fn render<T: Document>(document: &T, name: Option<&str>) -> Result<String, DataError> {
    let body = document.to_body().map_err(DataError::Invalid)?;
    let mut table = toml::Table::new();
    table.insert(
        FORMAT_KEY.to_owned(),
        toml::Value::Integer(i64::from(T::FORMAT)),
    );
    table.insert(KIND_KEY.to_owned(), toml::Value::String(T::KIND.to_owned()));
    if let Some(n) = name {
        table.insert(NAME_KEY.to_owned(), toml::Value::String(n.to_owned()));
    }
    for (k, v) in body {
        if table.contains_key(&k) {
            return Err(DataError::Invalid(format!("the key {k:?} is reserved")));
        }
        table.insert(k, v);
    }
    toml::to_string(&table).map_err(|e| DataError::Invalid(e.to_string()))
}

/// Reads a document file; `Ok(None)` if there is none.
pub fn read<T: Document>(path: &Path) -> Result<Option<Read<T>>, DataError> {
    match fs::read_to_string(path) {
        Ok(text) => parse(&text).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The copy kept of a file of an older format before it is first rewritten:
/// `<file>.format<N>.bak`.
pub fn backup_path(path: &Path, format: u32) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".format{format}.bak"));
    path.with_file_name(name)
}

/// The format of the document at `path`, if it is there and has one.
fn format_on_disk(path: &Path) -> Result<Option<u32>, DataError> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let Ok(table) = toml::from_str::<toml::Table>(&text) else {
        return Ok(None);
    };
    Ok(match table.get(FORMAT_KEY) {
        Some(toml::Value::Integer(v)) => u32::try_from(*v).ok(),
        _ => None,
    })
}

/// Writes a document file in one step. Refuses to replace a file of a newer format;
/// copies a file of an older format to [`backup_path`] once before replacing it.
pub fn write<T: Document>(path: &Path, document: &T, name: Option<&str>) -> Result<(), DataError> {
    let text = render(document, name)?;
    match format_on_disk(path)? {
        Some(found) if found > T::FORMAT => {
            return Err(DataError::Newer {
                found,
                supported: T::FORMAT,
            });
        }
        Some(found) if found < T::FORMAT => {
            let backup = backup_path(path, found);
            if !backup.exists() {
                fs::copy(path, &backup)?;
            }
        }
        _ => {}
    }
    write_atomic(path, text.as_bytes())
}

/// Writes `bytes` to `path` through a temporary file in the same directory, flushed to
/// disk and renamed over `path`, so a crash leaves the old file or the new one, never
/// half of one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), DataError> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(DataError::from)
}
