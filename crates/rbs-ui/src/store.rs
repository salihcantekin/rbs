//! Workspaces on disk: one TOML file per workspace in the user's config directory.
//!
//! Built-in workspaces exist without a file. Saving a changed built-in writes an
//! override file; restoring its default removes that file. Built-ins cannot be
//! deleted. Names are unique, compared case-insensitively. Asking the user to
//! confirm a delete is the shell's job.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::builtin;
use crate::workspace::{Workspace, WorkspaceError, same_name, validate_name};

/// Errors from the workspace store.
#[derive(Debug)]
pub enum StoreError {
    /// Reading or writing the directory failed.
    Io(io::Error),
    /// A file or workspace is invalid.
    Workspace(WorkspaceError),
    /// No workspace has this name.
    NotFound(String),
    /// Built-in workspaces cannot be deleted or renamed, only restored.
    BuiltIn(String),
    /// Another workspace already has this name.
    NameTaken(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "workspace folder: {e}"),
            StoreError::Workspace(e) => e.fmt(f),
            StoreError::NotFound(n) => write!(f, "no workspace named {n:?}"),
            StoreError::BuiltIn(n) => {
                write!(
                    f,
                    "{n:?} is built in; it can be restored, not deleted or renamed"
                )
            }
            StoreError::NameTaken(n) => write!(f, "a workspace named {n:?} already exists"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Io(e) => Some(e),
            StoreError::Workspace(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<WorkspaceError> for StoreError {
    fn from(e: WorkspaceError) -> Self {
        StoreError::Workspace(e)
    }
}

/// One entry of [`WorkspaceStore::list`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    /// Workspace name.
    pub name: String,
    /// Is it a built-in workspace?
    pub built_in: bool,
    /// For built-ins: has the user saved changes to it?
    pub modified: bool,
}

/// Result of listing the directory.
#[derive(Debug, Default)]
pub struct Listing {
    /// Built-ins first in their fixed order, then user workspaces by name.
    pub workspaces: Vec<Listed>,
    /// Files that could not be read, with the reason. They are left untouched.
    pub problems: Vec<(PathBuf, StoreError)>,
}

/// Workspace files in one directory.
#[derive(Clone, Debug)]
pub struct WorkspaceStore {
    dir: PathBuf,
}

const EXTENSION: &str = "toml";

/// Parsed files and unreadable files of the directory.
type Files = (Vec<(PathBuf, Workspace)>, Vec<(PathBuf, StoreError)>);

impl WorkspaceStore {
    /// A store in `dir`. The directory is created on the first save.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        WorkspaceStore { dir: dir.into() }
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every readable workspace file with its parsed contents.
    fn files(&self) -> Result<Files, StoreError> {
        let mut ok = Vec::new();
        let mut bad = Vec::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((ok, bad)),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
                continue;
            }
            match fs::read_to_string(&path)
                .map_err(StoreError::from)
                .and_then(|t| Workspace::from_toml(&t).map_err(StoreError::from))
            {
                Ok(w) => ok.push((path, w)),
                Err(e) => bad.push((path, e)),
            }
        }
        ok.sort_by(|a, b| a.0.cmp(&b.0));
        Ok((ok, bad))
    }

    fn file_of(&self, name: &str) -> Result<Option<PathBuf>, StoreError> {
        Ok(self
            .files()?
            .0
            .into_iter()
            .find(|(_, w)| same_name(&w.name, name))
            .map(|(p, _)| p))
    }

    /// All workspaces, and files that could not be read.
    pub fn list(&self) -> Result<Listing, StoreError> {
        let (files, problems) = self.files()?;
        let mut workspaces: Vec<Listed> = builtin::NAMES
            .iter()
            .map(|n| Listed {
                name: (*n).to_owned(),
                built_in: true,
                modified: files.iter().any(|(_, w)| same_name(&w.name, n)),
            })
            .collect();
        let mut user: Vec<Listed> = files
            .into_iter()
            .filter(|(_, w)| !builtin::is_built_in(&w.name))
            .map(|(_, w)| Listed {
                name: w.name,
                built_in: false,
                modified: false,
            })
            .collect();
        user.sort_by_key(|l| l.name.to_lowercase());
        user.dedup_by(|a, b| same_name(&a.name, &b.name));
        workspaces.extend(user);
        Ok(Listing {
            workspaces,
            problems,
        })
    }

    /// Load a workspace by name: the saved file if there is one, else the built-in default.
    pub fn load(&self, name: &str) -> Result<Workspace, StoreError> {
        if let Some(path) = self.file_of(name)? {
            let text = fs::read_to_string(path)?;
            return Ok(Workspace::from_toml(&text)?);
        }
        builtin::by_name(name).ok_or_else(|| StoreError::NotFound(name.to_owned()))
    }

    /// Save a workspace, replacing the file of the same name. The write is
    /// atomic: a crash leaves either the old or the new file.
    pub fn save(&self, workspace: &Workspace) -> Result<PathBuf, StoreError> {
        workspace.validate()?;
        let text = workspace.to_toml()?;
        let path = match self.file_of(&workspace.name)? {
            Some(p) => p,
            None => self.free_path(&workspace.name)?,
        };
        fs::create_dir_all(&self.dir)?;
        let tmp = path.with_extension("toml.tmp");
        {
            use std::io::Write;
            let mut f = fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            // Flush to disk before the rename, so a crash cannot leave an empty file.
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(path)
    }

    fn free_path(&self, name: &str) -> Result<PathBuf, StoreError> {
        let stem = slug(name);
        for n in 1u32.. {
            let file = if n == 1 {
                format!("{stem}.{EXTENSION}")
            } else {
                format!("{stem}-{n}.{EXTENSION}")
            };
            let path = self.dir.join(file);
            if !path.exists() {
                return Ok(path);
            }
        }
        Err(StoreError::NameTaken(name.to_owned()))
    }

    /// Save a copy of `workspace` under a new name.
    pub fn save_as(&self, workspace: &Workspace, name: &str) -> Result<Workspace, StoreError> {
        validate_name(name)?;
        if builtin::is_built_in(name) || self.file_of(name)?.is_some() {
            return Err(StoreError::NameTaken(name.to_owned()));
        }
        let mut copy = workspace.clone();
        copy.name = name.to_owned();
        self.save(&copy)?;
        Ok(copy)
    }

    /// Rename a user workspace.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), StoreError> {
        if builtin::is_built_in(from) {
            return Err(StoreError::BuiltIn(from.to_owned()));
        }
        let path = self
            .file_of(from)?
            .ok_or_else(|| StoreError::NotFound(from.to_owned()))?;
        let mut w = self.load(from)?;
        if same_name(from, to) {
            // Only the case changes: rewrite the same file.
            validate_name(to)?;
            w.name = to.to_owned();
            self.save(&w)?;
            return Ok(());
        }
        self.save_as(&w, to)?;
        fs::remove_file(path)?;
        Ok(())
    }

    /// Delete a user workspace. Built-ins cannot be deleted.
    pub fn delete(&self, name: &str) -> Result<(), StoreError> {
        if builtin::is_built_in(name) {
            return Err(StoreError::BuiltIn(name.to_owned()));
        }
        let path = self
            .file_of(name)?
            .ok_or_else(|| StoreError::NotFound(name.to_owned()))?;
        fs::remove_file(path)?;
        Ok(())
    }

    /// Restore a built-in workspace to its default and return it.
    pub fn restore_default(&self, name: &str) -> Result<Workspace, StoreError> {
        let default =
            builtin::by_name(name).ok_or_else(|| StoreError::NotFound(name.to_owned()))?;
        if let Some(path) = self.file_of(name)? {
            fs::remove_file(path)?;
        }
        Ok(default)
    }

    /// Import a workspace file's text under its own name, or `rename_to` when
    /// given. Placement details stay as in the file (an exported file has none).
    pub fn import(&self, text: &str, rename_to: Option<&str>) -> Result<Workspace, StoreError> {
        let w = Workspace::from_toml(text)?;
        let name = rename_to.unwrap_or(&w.name).to_owned();
        self.save_as(&w, &name)
    }
}

/// A file name stem for a workspace name: lowercase ASCII letters and digits
/// joined by `-`, or `workspace` when nothing is left.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("workspace");
    }
    out.truncate(48);
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::panel::id;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("rbs-ui-store-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn built_ins_without_files() {
        let d = TempDir::new("builtins");
        let s = WorkspaceStore::new(&d.0);
        let l = s.list().unwrap();
        let names: Vec<_> = l.workspaces.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, builtin::NAMES);
        assert!(l.workspaces.iter().all(|w| w.built_in && !w.modified));
        assert_eq!(s.load("minimal").unwrap(), builtin::minimal());
        assert!(matches!(s.load("Nope"), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn modify_and_restore_built_in() {
        let d = TempDir::new("restore");
        let s = WorkspaceStore::new(&d.0);
        let mut w = builtin::live();
        w.close(&id("rbs.stats")).unwrap();
        s.save(&w).unwrap();
        assert_eq!(s.load("Live").unwrap(), w);
        assert!(s.list().unwrap().workspaces[1].modified);
        assert!(matches!(s.delete("Live"), Err(StoreError::BuiltIn(_))));
        assert_eq!(s.restore_default("live").unwrap(), builtin::live());
        assert_eq!(s.load("Live").unwrap(), builtin::live());
    }

    #[test]
    fn user_workspaces_lifecycle() {
        let d = TempDir::new("user");
        let s = WorkspaceStore::new(&d.0);
        let mine = s.save_as(&builtin::minimal(), "My live / chat").unwrap();
        assert!(d.0.join("my-live-chat.toml").exists());
        assert!(matches!(
            s.save_as(&mine, "my LIVE / chat"),
            Err(StoreError::NameTaken(_))
        ));
        assert!(matches!(
            s.save_as(&mine, "Edit"),
            Err(StoreError::NameTaken(_))
        ));
        // A different name with the same slug gets its own file.
        s.save_as(&mine, "My live: chat").unwrap();
        assert!(d.0.join("my-live-chat-2.toml").exists());
        s.rename("My live: chat", "Second").unwrap();
        s.rename("second", "SECOND").unwrap();
        s.rename("SECOND", "Second").unwrap();
        s.save_as(&mine, "Çanlı").unwrap();
        assert!(matches!(
            s.save_as(&mine, "çanlı"),
            Err(StoreError::NameTaken(_))
        ));
        s.delete("ÇANLı").unwrap();
        let names: Vec<_> = s
            .list()
            .unwrap()
            .workspaces
            .into_iter()
            .filter(|w| !w.built_in)
            .map(|w| w.name)
            .collect();
        assert_eq!(names, ["My live / chat", "Second"]);
        s.delete("second").unwrap();
        assert!(matches!(s.delete("second"), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn broken_files_are_reported_not_lost() {
        let d = TempDir::new("broken");
        fs::create_dir_all(&d.0).unwrap();
        fs::write(d.0.join("bad.toml"), "format = 1\nname = ").unwrap();
        fs::write(d.0.join("notes.txt"), "ignored").unwrap();
        let s = WorkspaceStore::new(&d.0);
        let l = s.list().unwrap();
        assert_eq!(l.workspaces.len(), 4);
        assert_eq!(l.problems.len(), 1);
        assert!(d.0.join("bad.toml").exists());
    }

    #[test]
    fn import_of_an_export() {
        let d = TempDir::new("import");
        let s = WorkspaceStore::new(&d.0);
        let text = builtin::audio().exported().to_toml().unwrap();
        assert!(matches!(
            s.import(&text, None),
            Err(StoreError::NameTaken(_))
        ));
        let w = s.import(&text, Some("Audio from Ana")).unwrap();
        assert_eq!(s.load("Audio from Ana").unwrap(), w);
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Minimal live"), "minimal-live");
        assert_eq!(slug("  --Çok güzel!! "), "ok-g-zel");
        assert_eq!(slug("ışığ"), "workspace");
    }
}
