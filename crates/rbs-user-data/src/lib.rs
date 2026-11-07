//! RBS user data (ADR-0051): what the user makes and keeps, as versioned TOML documents
//! in the config directory.
//!
//! - [`document`]: one file per document with `format` and `kind`; newer formats are
//!   refused and never overwritten, older ones upgraded in memory and backed up before
//!   the first rewrite; writes replace the file in one step.
//! - [`library`]: a directory of named entries of one kind (stable ids as file names,
//!   unique names, create, rename, duplicate, delete to `deleted/`, import, export).
//! - [`OutputProfile`] (glossary: Output profile) and the scene collection document
//!   ([`rbs_scene::SceneCollection`] implements [`Document`]).
//! - [`Current`]: which profile and collection are in use.
//! - [`theme`]: user themes (a library of `rbs_ui::tokens::ThemeBody`) and the
//!   [`Appearance`] choice (ADR-0057).
//! - [`UserData`]: where each of them lives under the config directory.
//!
//! Application layer: used by the composition roots (`rbs-app`, `rbs-cli`); no engine,
//! adapter or toolkit crate. Spec: `docs/specs/user-data.md`.

pub mod current;
pub mod document;
pub mod library;
pub mod profile;
pub mod scenes;
pub mod theme;

use std::path::{Path, PathBuf};

pub use current::Current;
pub use document::{DataError, Document};
pub use library::{Entry, EntryId, Library, Listed, Listing};
pub use profile::OutputProfile;
pub use theme::{Appearance, AppearanceMode, ThemeId};

/// Directory of output profiles, under the config directory.
pub const PROFILES_DIR: &str = "profiles";
/// Directory of scene collections.
pub const SCENE_COLLECTIONS_DIR: &str = "scenes";
/// File naming the profile and collection in use.
pub const CURRENT_FILE: &str = "current.toml";
/// Reserved: UI workspaces (ADR-0017, owned by `rbs-ui`).
pub const WORKSPACES_DIR: &str = "workspaces";
/// User themes (ADR-0057).
pub const THEMES_DIR: &str = "themes";
/// File holding the appearance choice (ADR-0057).
pub const APPEARANCE_FILE: &str = "appearance.toml";
/// Reserved: the asset library (format to be decided).
pub const LIBRARY_DIR: &str = "library";

/// The user data under one config directory.
#[derive(Debug, Clone)]
pub struct UserData {
    root: PathBuf,
}

impl UserData {
    /// The user data in `config` (normally `rbs_remote_settings::config_dir()`).
    pub fn new(config: impl Into<PathBuf>) -> Self {
        Self {
            root: config.into(),
        }
    }

    /// The config directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The output profiles.
    pub fn profiles(&self) -> Library<OutputProfile> {
        Library::new(self.root.join(PROFILES_DIR))
    }

    /// The scene collections.
    pub fn scene_collections(&self) -> Library<rbs_scene::SceneCollection> {
        Library::new(self.root.join(SCENE_COLLECTIONS_DIR))
    }

    /// The user themes.
    pub fn themes(&self) -> Library<rbs_ui::tokens::ThemeBody> {
        Library::new(self.root.join(THEMES_DIR))
    }

    /// The appearance choice; the defaults if the file is missing, or if it cannot be
    /// read (with the reason; the file is left as it is).
    pub fn appearance(&self) -> (Appearance, Option<DataError>) {
        match document::read::<Appearance>(&self.root.join(APPEARANCE_FILE)) {
            Ok(Some(r)) => (r.document, None),
            Ok(None) => (Appearance::default(), None),
            Err(e) => (Appearance::default(), Some(e)),
        }
    }

    /// Records the appearance choice.
    pub fn set_appearance(
        &self,
        _lock: &WriterLock,
        appearance: &Appearance,
    ) -> Result<(), DataError> {
        document::write(&self.root.join(APPEARANCE_FILE), appearance, None)
    }

    /// Moves `appearance.toml` aside to `appearance.toml.reset.bak` (numbered when
    /// taken), so the app starts with the default look (`rbs-app --reset-appearance`).
    /// Returns where it went; `None` when there was no file.
    pub fn reset_appearance(&self, _lock: &WriterLock) -> Result<Option<PathBuf>, DataError> {
        let path = self.root.join(APPEARANCE_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let to = document::aside_path(&path, "reset");
        std::fs::rename(&path, &to)?;
        Ok(Some(to))
    }

    /// Which profile and collection are in use; unset if the file is missing or cannot
    /// be read (with the reason).
    pub fn current(&self) -> (Current, Option<DataError>) {
        match document::read::<Current>(&self.root.join(CURRENT_FILE)) {
            Ok(Some(r)) => (r.document, None),
            Ok(None) => (Current::default(), None),
            Err(e) => (Current::default(), Some(e)),
        }
    }

    /// Records which profile and collection are in use.
    pub fn set_current(&self, _lock: &WriterLock, current: &Current) -> Result<(), DataError> {
        document::write(&self.root.join(CURRENT_FILE), current, None)
    }

    /// Takes the writer lock (`user-data.lock`), or `Ok(None)` if another program
    /// holds it. Only the holder may write user data; a second app instance opens it
    /// read-only, and `rbs-cli` only reads (ADR-0051). Released when dropped or when
    /// the process ends.
    pub fn try_lock(&self) -> Result<Option<WriterLock>, DataError> {
        std::fs::create_dir_all(&self.root)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join(LOCK_FILE))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(WriterLock { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }
}

/// File whose lock marks the program allowed to write user data.
pub const LOCK_FILE: &str = "user-data.lock";

/// Held by the one program that writes user data ([`UserData::try_lock`]).
#[derive(Debug)]
pub struct WriterLock {
    _file: std::fs::File,
}
