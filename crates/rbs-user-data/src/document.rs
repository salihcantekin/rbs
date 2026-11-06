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
use std::sync::atomic::{AtomicU64, Ordering};

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
#[non_exhaustive]
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
    // A newer RBS may use syntax this build cannot parse; say "newer", not "broken".
    if let Some(found) = scan_format(text)
        && found > T::FORMAT
    {
        return Err(DataError::Newer {
            found,
            supported: T::FORMAT,
        });
    }
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
    let text = toml::to_string(&table).map_err(|e| DataError::Invalid(e.to_string()))?;
    // Never write what could not be read back.
    if text.len() as u64 > MAX_FILE {
        return Err(DataError::Invalid(format!(
            "the document would be {} bytes; at most {MAX_FILE} are allowed",
            text.len()
        )));
    }
    Ok(text)
}

/// Largest document file read, in bytes. Shared files are untrusted input; a scene
/// collection with thousands of objects is well below this.
pub const MAX_FILE: u64 = 16 * 1024 * 1024;

/// The text of a file; `Ok(None)` if there is none. Files larger than [`MAX_FILE`] are
/// refused.
fn read_text(path: &Path) -> Result<Option<String>, DataError> {
    let f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let len = f.metadata()?.len();
    if len > MAX_FILE {
        return Err(DataError::Invalid(format!(
            "the file is {len} bytes; at most {MAX_FILE} are read"
        )));
    }
    let mut text = String::new();
    io::Read::read_to_string(&mut io::Read::take(f, MAX_FILE + 1), &mut text)?;
    Ok(Some(text))
}

/// The text of a file of any size (the file about to be replaced); `Ok(None)` if
/// there is none. Not valid UTF-8 counts as unreadable, so it is moved aside.
/// A file larger than [`MAX_FILE`] (say, a video exported over by mistake) is not read
/// whole: only its first bytes, enough for [`scan_format`].
fn read_text_unbounded(path: &Path) -> Result<Option<String>, DataError> {
    let f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let limit = if f.metadata()?.len() > MAX_FILE {
        HEAD_BYTES
    } else {
        MAX_FILE
    };
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut io::Read::take(f, limit), &mut bytes)?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// How much of an oversized file is read to look for its format line.
const HEAD_BYTES: u64 = 4096;

/// Reads a document file; `Ok(None)` if there is none.
pub fn read<T: Document>(path: &Path) -> Result<Option<Read<T>>, DataError> {
    match read_text(path)? {
        Some(text) => parse(&text).map(Some),
        None => Ok(None),
    }
}

/// A document's envelope: what [`peek`] reads without the body.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Its format.
    pub format: u32,
    /// Its kind.
    pub kind: String,
    /// Its name, if any.
    pub name: Option<String>,
}

/// The `format` of a document text, found without parsing the rest: the first
/// top-level `format = <integer>` line ([`render`] always writes it first). Lets a
/// file that a newer RBS wrote in syntax this build cannot parse still be recognised
/// as newer.
pub fn scan_format(text: &str) -> Option<u32> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            return None;
        }
        if let Some(rest) = line.strip_prefix(FORMAT_KEY)
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            let digits: String = value
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            return digits.parse().ok();
        }
    }
    None
}

/// Reads only the envelope of a document file (`format`, `kind`, `name`), checking
/// the kind and refusing newer formats, without building the body. `Ok(None)` if
/// there is no file. A file whose body is broken can pass; [`read`] reports it.
pub fn peek<T: Document>(path: &Path) -> Result<Option<Envelope>, DataError> {
    let Some(text) = read_text(path)? else {
        return Ok(None);
    };
    if let Some(found) = scan_format(&text)
        && found > T::FORMAT
    {
        return Err(DataError::Newer {
            found,
            supported: T::FORMAT,
        });
    }
    // Files RBS writes have the envelope before the first table: parse only that part
    // (listing a library of large collections stays fast). Hand-written files may
    // order keys differently; then the whole file is parsed.
    let head: String = text
        .lines()
        .take_while(|l| !l.trim_start().starts_with('['))
        .flat_map(|l| [l, "\n"])
        .collect();
    let table = match toml::from_str::<toml::Table>(&head) {
        Ok(t)
            if [FORMAT_KEY, KIND_KEY, NAME_KEY]
                .iter()
                .all(|k| t.contains_key(*k)) =>
        {
            t
        }
        _ => {
            toml::from_str(&text).map_err(|e: toml::de::Error| DataError::Syntax(e.to_string()))?
        }
    };
    let format = match table.get(FORMAT_KEY) {
        Some(toml::Value::Integer(v)) => u32::try_from(*v).map_err(|_| DataError::NoVersion)?,
        _ => return Err(DataError::NoVersion),
    };
    let kind = match table.get(KIND_KEY) {
        Some(toml::Value::String(k)) => k.clone(),
        _ => return Err(DataError::NoVersion),
    };
    if kind != T::KIND {
        return Err(DataError::WrongKind {
            found: kind,
            expected: T::KIND,
        });
    }
    if format == 0 {
        return Err(DataError::NoVersion);
    }
    if format > T::FORMAT {
        return Err(DataError::Newer {
            found: format,
            supported: T::FORMAT,
        });
    }
    let name = match table.get(NAME_KEY) {
        None => None,
        Some(toml::Value::String(n)) => Some(n.clone()),
        Some(_) => return Err(DataError::Invalid("name must be text".to_owned())),
    };
    Ok(Some(Envelope { format, kind, name }))
}

/// The copy kept of a file of an older format before it is first rewritten:
/// `<file>.format<N>.bak`.
pub fn backup_path(path: &Path, format: u32) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".format{format}.bak"));
    path.with_file_name(name)
}

/// Where a file that could not be read is moved before it is replaced:
/// `<file>.unreadable.bak`, or `<file>.unreadable-N.bak` if that is taken.
pub fn unreadable_path(path: &Path) -> PathBuf {
    let base = path.file_name().unwrap_or_default().to_os_string();
    let mut n = 1u32;
    loop {
        let mut name = base.clone();
        if n == 1 {
            name.push(".unreadable.bak");
        } else {
            name.push(format!(".unreadable-{n}.bak"));
        }
        let candidate = path.with_file_name(name);
        if !candidate.exists() || n == u32::MAX {
            return candidate;
        }
        n += 1;
    }
}

/// Writes a document file in one step, without ever losing the file it replaces:
///
/// - a file of a newer format is refused ([`DataError::Newer`]) and left untouched,
///   even when this build cannot parse it ([`scan_format`]);
/// - a file of an older format is copied to [`backup_path`] once;
/// - a file that does not load as a document of this kind (broken by hand, also only
///   in its body, or another kind) is moved to [`unreadable_path`] first.
pub fn write<T: Document>(path: &Path, document: &T, name: Option<&str>) -> Result<(), DataError> {
    let text = render(document, name)?;
    if let Some(old) = read_text_unbounded(path)? {
        // The whole document, not only its envelope: a hand edit that breaks the body
        // (a typo'd key, a broken invariant) must be kept too.
        match peek_text::<T>(&old).and_then(|f| parse::<T>(&old).map(|_| f)) {
            Ok(found) if found < T::FORMAT => {
                let backup = backup_path(path, found);
                if !backup.exists() {
                    write_atomic(&backup, old.as_bytes())?;
                }
            }
            Ok(_) => {}
            Err(e @ DataError::Newer { .. }) => return Err(e),
            Err(_) => fs::rename(path, unreadable_path(path))?,
        }
    }
    write_atomic(path, text.as_bytes())
}

/// The format of a document text of kind `T`, or why it is not one.
fn peek_text<T: Document>(text: &str) -> Result<u32, DataError> {
    if let Some(found) = scan_format(text)
        && found > T::FORMAT
    {
        return Err(DataError::Newer {
            found,
            supported: T::FORMAT,
        });
    }
    let table: toml::Table =
        toml::from_str(text).map_err(|e: toml::de::Error| DataError::Syntax(e.to_string()))?;
    let format = match table.get(FORMAT_KEY) {
        Some(toml::Value::Integer(v)) if *v >= 1 => {
            u32::try_from(*v).map_err(|_| DataError::NoVersion)?
        }
        _ => return Err(DataError::NoVersion),
    };
    match table.get(KIND_KEY) {
        Some(toml::Value::String(k)) if k == T::KIND => {}
        _ => return Err(DataError::NoVersion),
    }
    if format > T::FORMAT {
        return Err(DataError::Newer {
            found: format,
            supported: T::FORMAT,
        });
    }
    Ok(format)
}

/// Writes `bytes` to `path` through a temporary file of its own in the same directory
/// (unique per process and call, created new), flushed to disk and renamed over
/// `path`, so a crash leaves the old file or the new one, never half of one, and two
/// writers never share a temporary file. On Unix the directory is flushed too, so the
/// rename survives a power loss. On Windows a rename refused because another program
/// (an indexer, antivirus, an editor) briefly holds the file is tried again a few
/// times.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), DataError> {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    if let Some(dir) = dir {
        fs::create_dir_all(dir)?;
    }
    let n = WRITES.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        rename_replacing(&tmp, path)?;
        #[cfg(unix)]
        if let Some(dir) = dir {
            fs::File::open(dir)?.sync_all()?;
        }
        Ok::<(), io::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(DataError::from)
}

fn rename_replacing(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        // ERROR_ACCESS_DENIED (5) and ERROR_SHARING_VIOLATION (32): another program
        // has the target open for a moment. Not a hot path; a short wait is fine.
        for _ in 0..10 {
            match fs::rename(from, to) {
                Err(e) if matches!(e.raw_os_error(), Some(5 | 32)) => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                other => return other,
            }
        }
    }
    fs::rename(from, to)
}
