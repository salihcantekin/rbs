//! The recent targets of the Source tab (ADR-0077 decision 3a): the last targets the user
//! picked for each source type, newest first, at most [`CAP`] per type, kept in the
//! user's settings (`recent-targets.toml` in the config folder). Only the target's
//! adapter, id and name are kept, never a picture or a path. The host lists them only
//! while they still exist.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FORMAT: u32 = 1;

/// The most recent targets kept per source type.
pub const CAP: usize = 5;

/// One recent target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recent {
    /// The adapter that listed it.
    pub adapter: String,
    /// Its id for that adapter.
    pub target: String,
    /// Its name when it was picked.
    pub name: String,
}

/// The recent targets of every source type; the key is the type's id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentTargets {
    /// The file's format; 1.
    pub format_version: u32,
    /// Newest first.
    pub types: BTreeMap<String, Vec<Recent>>,
}

impl Default for RecentTargets {
    fn default() -> Self {
        Self {
            format_version: FORMAT,
            types: BTreeMap::new(),
        }
    }
}

impl RecentTargets {
    /// The recent targets of `kind`, newest first.
    pub fn list(&self, kind: &str) -> &[Recent] {
        self.types.get(kind).map_or(&[], Vec::as_slice)
    }

    /// Puts a target first for `kind` (it moves up when it is already there) and keeps the
    /// newest [`CAP`]. Returns whether the list changed.
    pub fn push(&mut self, kind: &str, adapter: &str, target: &str, name: &str) -> bool {
        let list = self.types.entry(kind.to_owned()).or_default();
        if list
            .first()
            .is_some_and(|r| r.adapter == adapter && r.target == target && r.name == name)
        {
            return false;
        }
        list.retain(|r| !(r.adapter == adapter && r.target == target));
        list.insert(
            0,
            Recent {
                adapter: adapter.to_owned(),
                target: target.to_owned(),
                name: name.to_owned(),
            },
        );
        list.truncate(CAP);
        true
    }

    /// Forgets the recent targets of `kind`. Returns whether there were any.
    pub fn clear(&mut self, kind: &str) -> bool {
        self.types.remove(kind).is_some_and(|l| !l.is_empty())
    }
}

fn file(config: &Path) -> PathBuf {
    config.join("recent-targets.toml")
}

/// The saved list; empty when there is none. A file that cannot be read or is of a newer
/// format gives an empty list and the reason (the file is left as it is until a target is
/// picked).
pub fn load(config: &Path) -> (RecentTargets, Option<String>) {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (RecentTargets::default(), None);
        }
        Err(e) => {
            return (
                RecentTargets::default(),
                Some(format!("{}: {e}", path.display())),
            );
        }
    };
    match toml::from_str::<RecentTargets>(&text) {
        Ok(r) if r.format_version != FORMAT => (
            RecentTargets::default(),
            Some(format!(
                "{}: format version {} is not supported",
                path.display(),
                r.format_version
            )),
        ),
        Ok(mut r) => {
            for list in r.types.values_mut() {
                list.truncate(CAP);
            }
            (r, None)
        }
        Err(e) => (
            RecentTargets::default(),
            Some(format!("{}: {e}", path.display())),
        ),
    }
}

/// Writes the list (replacing the file in one step).
pub fn save(config: &Path, recent: &RecentTargets) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(recent).map_err(std::io::Error::other)?;
    let path = file(config);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn the_newest_target_is_first_and_a_repeat_moves_up() {
        let mut r = RecentTargets::default();
        assert!(r.push("window", "a", "1", "One"));
        assert!(r.push("window", "a", "2", "Two"));
        assert!(r.push("window", "a", "1", "One"));
        let ids: Vec<_> = r.list("window").iter().map(|x| x.target.as_str()).collect();
        assert_eq!(ids, ["1", "2"]);
        assert!(!r.push("window", "a", "1", "One"), "already first");
    }

    #[test]
    fn at_most_five_are_kept_per_type() {
        let mut r = RecentTargets::default();
        for i in 0..8 {
            r.push("camera", "a", &i.to_string(), "Cam");
        }
        assert_eq!(r.list("camera").len(), CAP);
        assert_eq!(r.list("camera")[0].target, "7");
        assert!(r.list("window").is_empty(), "types are separate");
    }

    #[test]
    fn clear_forgets_one_type() {
        let mut r = RecentTargets::default();
        r.push("window", "a", "1", "One");
        r.push("camera", "a", "2", "Two");
        assert!(r.clear("window"));
        assert!(!r.clear("window"));
        assert_eq!(r.list("camera").len(), 1);
    }

    #[test]
    fn the_list_survives_a_save_and_a_load() {
        let dir = std::env::temp_dir().join(format!("rbs-recent-{}", std::process::id()));
        let mut r = RecentTargets::default();
        r.push("window", "a", "1", "One");
        save(&dir, &r).unwrap();
        let (loaded, note) = load(&dir);
        assert_eq!(note, None);
        assert_eq!(loaded, r);
        std::fs::write(file(&dir), "format_version = 9\n[types]\n").unwrap();
        let (empty, note) = load(&dir);
        assert!(empty.types.is_empty() && note.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
