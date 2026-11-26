//! Per-collection view state (`view-state/<collection id>.toml`, `kind =
//! "view-state"`, format 1): the Scenes and Sources list filters (ADR-0195 decision 6).
//! It is not part of the scene collection or of a workspace. A collection without a
//! file shows everything. Spec: `docs/specs/user-data.md`, "View state".

use serde::{Deserialize, Serialize};

use crate::document::{self, BodyError, Document};

/// Largest view-state file, in bytes.
pub const MAX_VIEW_STATE_FILE: u64 = 16 * 1024;

/// Most source categories one file may name.
pub const MAX_HIDDEN_CATEGORIES: usize = 64;

/// The list filters of one scene collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelFilters {
    /// Scenes panel: list the inactive scenes.
    #[serde(default = "yes")]
    pub show_inactive_scenes: bool,
    /// Sources panel: list the hidden sources.
    #[serde(default = "yes")]
    pub show_hidden_sources: bool,
    /// Sources panel: the category ids left out of the list. Names, not the shown
    /// ones, so a category added later is shown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_source_categories: Vec<String>,
}

fn yes() -> bool {
    true
}

impl Default for PanelFilters {
    fn default() -> Self {
        Self {
            show_inactive_scenes: true,
            show_hidden_sources: true,
            hidden_source_categories: Vec::new(),
        }
    }
}

impl Document for PanelFilters {
    const KIND: &'static str = "view-state";
    const FORMAT: u32 = 1;
    const MAX_SIZE: u64 = MAX_VIEW_STATE_FILE;

    fn from_body(body: toml::Table) -> Result<Self, BodyError> {
        let f: PanelFilters = document::from_body(body)?;
        f.check()?;
        Ok(f)
    }

    fn to_body(&self) -> Result<toml::Table, BodyError> {
        self.check()?;
        document::body_of(self)
    }
}

impl PanelFilters {
    fn check(&self) -> Result<(), BodyError> {
        if self.hidden_source_categories.len() > MAX_HIDDEN_CATEGORIES {
            return Err(BodyError::Value(format!(
                "more than {MAX_HIDDEN_CATEGORIES} hidden source categories"
            )));
        }
        let bad = |c: &String| {
            c.is_empty() || c.len() > 32 || !c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        };
        if let Some(c) = self.hidden_source_categories.iter().find(|c| bad(c)) {
            return Err(BodyError::Value(format!("{c:?} is not a category id")));
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::{EntryId, UserData};

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-view-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_collection_without_a_file_shows_everything_and_writes_round_trip() {
        let d = dir("round-trip");
        let data = UserData::new(&d);
        let id = EntryId::parse("show").unwrap();
        let (f, err) = data.view_state(&id);
        assert_eq!(f, PanelFilters::default());
        assert!(err.is_none());
        let lock = data.try_lock().unwrap().unwrap();
        let set = PanelFilters {
            show_inactive_scenes: false,
            show_hidden_sources: false,
            hidden_source_categories: vec!["text".into(), "window".into()],
        };
        data.set_view_state(&lock, &id, &set).unwrap();
        assert_eq!(data.view_state(&id).0, set);
        data.delete_view_state(&lock, &id).unwrap();
        assert_eq!(data.view_state(&id).0, PanelFilters::default());
        // Nothing to delete is not an error.
        data.delete_view_state(&lock, &id).unwrap();
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn a_bad_category_is_refused() {
        let mut f = PanelFilters::default();
        f.hidden_source_categories.push("Not An Id".into());
        assert!(f.to_body().is_err());
    }
}
