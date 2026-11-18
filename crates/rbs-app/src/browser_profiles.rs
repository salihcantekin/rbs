//! Kept browser profiles (ADR-0132 decision 10): the random `profile_id` of a browser
//! source with `keep_signed_in`, and its directory under `browser-profiles/` in the
//! local data directory.
//!
//! - A new id is made when `keep_signed_in` turns on, for a copy of a source (Duplicate
//!   source, Duplicate scene with new sources), for every source of a copied or
//!   imported collection, and for a loaded source whose id another source already
//!   holds (the one that held it first keeps it).
//! - Forget sign-in gives the source a new id, so its page starts again signed out at
//!   once, and deletes the old directory once the page that used it has let go.
//! - At app start, directories that no saved collection names are deleted; the sweep
//!   is skipped when any collection cannot be read.
//!
//! Nothing here runs while no browser source keeps a profile (P13).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rbs_scene::SceneCollection;
use rbs_scene::model::{SettingValue, SourceId};
use rbs_sources::types::{BROWSER_SOURCE, keys};

/// `browser-profiles/` beside the add-ons in the local data directory.
pub fn dir_beside(add_ons_root: &Path) -> Option<PathBuf> {
    add_ons_root.parent().map(|data| data.join(DIR_NAME))
}

/// The name of the profiles directory in the local data directory.
pub const DIR_NAME: &str = "browser-profiles";

/// The start-up sweep on a thread of its own, when `dir` holds anything: `referenced`
/// is asked only then (it reads every saved collection) and `None` skips the sweep.
pub fn sweep_at_start(
    dir: PathBuf,
    referenced: impl FnOnce() -> Option<BTreeSet<String>>,
) {
    let has_any = std::fs::read_dir(&dir).is_ok_and(|mut e| e.next().is_some());
    if !has_any {
        return;
    }
    let Some(referenced) = referenced() else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("rbs-profile-sweep".into())
        .spawn(move || {
            sweep(&dir, &referenced);
        });
    drop(spawned);
}

/// A new random profile id: 32 lowercase hex digits (128 bits).
pub fn new_id() -> Result<String, String> {
    let token =
        rbs_api::token::Token::generate().map_err(|e| format!("No random id for the profile: {e}."))?;
    Ok(token.hash().to_hex().chars().take(32).collect())
}

/// The profile id a browser source keeps, if it has a valid one.
pub fn id_of(source: &rbs_scene::model::Source) -> Option<&str> {
    if source.type_id.as_str() != BROWSER_SOURCE {
        return None;
    }
    match source.settings.get(keys::PROFILE_ID) {
        Some(SettingValue::Text(id)) if rbs_sources::is_profile_id(id) => Some(id),
        _ => None,
    }
}

/// Every profile id `collection` names.
pub fn ids(collection: &SceneCollection) -> BTreeSet<String> {
    collection
        .sources()
        .filter_map(id_of)
        .map(str::to_owned)
        .collect()
}

/// True when some browser source of `collection` has a profile id or keeps signed in.
pub fn keeps_any(collection: &SceneCollection) -> bool {
    collection.sources().any(|s| {
        s.type_id.as_str() == BROWSER_SOURCE
            && (s.settings.contains_key(keys::PROFILE_ID)
                || s.settings.get(keys::KEEP_SIGNED_IN) == Some(&SettingValue::Bool(true)))
    })
}

/// Gives a new id to each browser source of `collection` whose id is in `taken` or
/// already held by a source with a lower id, or that keeps signed in without one; with
/// `all`, to every source that has one (a copied or imported collection). The change is not an undo step: the caller does
/// this before the collection runs. Returns how many ids changed.
pub fn renew(
    collection: &mut SceneCollection,
    taken: &BTreeSet<String>,
    all: bool,
) -> Result<usize, String> {
    let mut seen = BTreeSet::new();
    let mut renew: Vec<SourceId> = Vec::new();
    for source in collection.sources() {
        let Some(id) = id_of(source) else {
            // Kept signed in without an id (an imported or exported file): one now.
            if source.type_id.as_str() == BROWSER_SOURCE
                && source.settings.get(keys::KEEP_SIGNED_IN) == Some(&SettingValue::Bool(true))
            {
                renew.push(source.id);
            }
            continue;
        };
        if all || taken.contains(id) || !seen.insert(id.to_owned()) {
            renew.push(source.id);
        }
    }
    for id in &renew {
        let Some(mut settings) = collection.source(*id).map(|s| s.settings.clone()) else {
            continue;
        };
        settings.insert(keys::PROFILE_ID.to_owned(), SettingValue::Text(new_id()?));
        collection
            .set_source_settings(*id, settings)
            .map_err(|e| e.to_string())?;
    }
    Ok(renew.len())
}

/// Removes every kept-profile id from `collection`, for a file that leaves this
/// computer (an export, ADR-0132 decision 10). The sources keep `keep_signed_in`; a
/// collection imported from the file gets new ids when the switch is next turned on.
pub fn strip(collection: &mut SceneCollection) {
    let kept: Vec<SourceId> = collection
        .sources()
        .filter(|s| {
            s.type_id.as_str() == BROWSER_SOURCE && s.settings.contains_key(keys::PROFILE_ID)
        })
        .map(|s| s.id)
        .collect();
    for id in kept {
        if let Some(mut settings) = collection.source(id).map(|s| s.settings.clone()) {
            settings.remove(keys::PROFILE_ID);
            let _ = collection.set_source_settings(id, settings);
        }
    }
}

/// Deletes the profile directory `id` under `dir` on a thread of its own: the page that
/// used it may still be closing (on Windows its files stay locked until it has), so it
/// tries again for a few seconds. A directory still there after that goes with the
/// next start's sweep. Only a valid id names a directory.
pub fn remove_soon(dir: &Path, id: &str) {
    if !rbs_sources::is_profile_id(id) {
        return;
    }
    let path = dir.join(id);
    let spawned = std::thread::Builder::new()
        .name("rbs-profile-remove".into())
        .spawn(move || remove_with_retries(&path, 25, Duration::from_millis(200)));
    // Without a thread the directory goes with the next start's sweep.
    drop(spawned);
}

/// Removes `path`, trying `tries` times `pause` apart. True once it is gone.
fn remove_with_retries(path: &Path, tries: u32, pause: Duration) -> bool {
    for attempt in 0..tries {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) if attempt + 1 == tries => {}
            // Bounded retry of a file the closing page still holds, on a thread of its
            // own; never in a test (tests pass `tries` 1).
            Err(_) => std::thread::sleep(pause),
        }
    }
    false
}

/// The start-up sweep: deletes every profile directory under `dir` whose name is a
/// profile id that `referenced` does not hold. Other entries are left alone. Returns
/// the directories removed.
pub fn sweep(dir: &Path, referenced: &BTreeSet<String>) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !rbs_sources::is_profile_id(name) || referenced.contains(name) {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) && remove_with_retries(&entry.path(), 1, Duration::ZERO) {
            removed.push(entry.path());
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use rbs_graph::node::NodeTypeId;
    use rbs_scene::model::Settings;

    fn page(c: &mut SceneCollection, name: &str, id: Option<&str>) -> SourceId {
        let mut settings = Settings::new();
        settings.insert(keys::URL.into(), SettingValue::Text("https://example.com".into()));
        if let Some(id) = id {
            settings.insert(keys::KEEP_SIGNED_IN.into(), SettingValue::Bool(true));
            settings.insert(keys::PROFILE_ID.into(), SettingValue::Text(id.into()));
        }
        let (sid, _) = c
            .add_source(name, NodeTypeId::new_static(BROWSER_SOURCE), settings)
            .unwrap();
        sid
    }

    const A: &str = "0123456789abcdef0123456789abcdef";
    const B: &str = "fedcba9876543210fedcba9876543210";

    #[test]
    fn a_new_id_is_a_profile_id() {
        let id = new_id().unwrap();
        assert!(rbs_sources::is_profile_id(&id), "{id}");
        assert_ne!(id, new_id().unwrap());
    }

    #[test]
    fn the_first_holder_keeps_a_shared_id_and_the_later_one_gets_a_new_one() {
        let mut c = SceneCollection::new();
        let first = page(&mut c, "First", Some(A));
        let second = page(&mut c, "Second", Some(A));
        let plain = page(&mut c, "Plain", None);
        assert_eq!(renew(&mut c, &BTreeSet::new(), false).unwrap(), 1);
        assert_eq!(id_of(c.source(first).unwrap()), Some(A));
        let new = id_of(c.source(second).unwrap()).unwrap();
        assert_ne!(new, A);
        assert!(id_of(c.source(plain).unwrap()).is_none());
    }

    #[test]
    fn an_id_another_collection_holds_is_replaced_and_all_renews_every_one() {
        let mut c = SceneCollection::new();
        let a = page(&mut c, "A", Some(A));
        let b = page(&mut c, "B", Some(B));
        let taken = BTreeSet::from([A.to_owned()]);
        assert_eq!(renew(&mut c, &taken, false).unwrap(), 1);
        assert_ne!(id_of(c.source(a).unwrap()), Some(A));
        assert_eq!(id_of(c.source(b).unwrap()), Some(B));
        assert_eq!(renew(&mut c, &BTreeSet::new(), true).unwrap(), 2);
        assert_ne!(id_of(c.source(b).unwrap()), Some(B));
        assert_eq!(ids(&c).len(), 2);
    }

    #[test]
    fn the_sweep_removes_only_unreferenced_profile_directories() {
        let dir = std::env::temp_dir().join(format!("rbs-profiles-{}", new_id().unwrap()));
        for name in [A, B, "not-a-profile"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join("Cookies"), b"x").unwrap();
        }
        let removed = sweep(&dir, &BTreeSet::from([A.to_owned()]));
        assert_eq!(removed, vec![dir.join(B)]);
        assert!(dir.join(A).exists() && dir.join("not-a-profile").exists());
        assert!(!dir.join(B).exists());
        assert!(remove_with_retries(&dir.join(A), 1, Duration::ZERO));
        assert!(remove_with_retries(&dir.join(A), 1, Duration::ZERO), "gone counts");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
