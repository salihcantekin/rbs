//! Loading another output profile or scene collection on the writer thread, so a
//! switch never waits on the disk on the UI thread (docs/architecture/threads.md).
//! The job is queued behind every save asked for so far, so it reads what they wrote;
//! the window awaits the [`Pending`] and then uses what was loaded.

use std::collections::BTreeSet;
use std::sync::Arc;

use rbs_scene::SceneCollection;
use rbs_user_data::{EntryId, OutputProfile, UserData};

use super::entries;
use super::later::Pending;
use super::{Job, Names, SessionError, Shared, UserSession};
use crate::canvas_profile::CanvasProfile;

/// An output profile read for a switch; [`UserSession::use_profile`] makes it the one
/// in use.
pub struct LoadedProfile {
    id: EntryId,
    name: String,
    document: OutputProfile,
}

impl LoadedProfile {
    /// The profile's id.
    pub fn id(&self) -> &EntryId {
        &self.id
    }
}

/// A scene collection read for a switch: its id, its name and what it holds.
pub struct LoadedCollection {
    /// The collection's id.
    pub id: EntryId,
    /// Its name.
    pub name: String,
    /// The collection, with kept sign-ins another collection holds signed out.
    pub collection: SceneCollection,
}

impl UserSession {
    /// Reads the output profile `id` after every save asked for so far, without
    /// waiting. A read-only session has no writer and reads it at once.
    pub fn load_profile_later(&self, id: &EntryId) -> Pending<LoadedProfile> {
        let task = load_profile(id.clone());
        match &self.saver {
            Some(_) => self.later(move |data, _, _| task(data)),
            None => Pending::ready(task(&self.data)),
        }
    }

    /// Makes an output profile as [`UserSession::new_profile`] does, then reads it
    /// for a switch, without waiting.
    pub fn new_profile_loaded_later(&self, name: &str, copy: bool) -> Pending<LoadedProfile> {
        let made = entries::new_profile(name, copy.then(|| self.profile_id.clone()));
        self.later(move |data, lock, names| {
            let id = made(data, lock, names)?;
            load_profile(id)(data)
        })
    }

    /// Uses the profile `loaded` from now on (its sections replace
    /// [`UserSession::profile`]) and saves the choice.
    pub fn use_profile(&mut self, loaded: LoadedProfile) {
        if let Some(s) = &self.saver {
            Names::set(&s.shared.names.profiles, &loaded.id, &loaded.name);
        }
        self.profile_id = loaded.id;
        self.profile_name = loaded.name;
        self.profile = loaded.document;
        self.save_current();
    }

    /// Reads the scene collection `id` after every save asked for so far, to run it,
    /// without waiting. A read-only session reads it at once.
    pub fn load_collection_later(&self, id: &EntryId) -> Pending<LoadedCollection> {
        let task = load_collection(id.clone(), self.shared());
        match &self.saver {
            Some(_) => self.later(move |data, _, _| task(data)),
            None => Pending::ready(task(&self.data)),
        }
    }

    /// Makes a scene collection as [`UserSession::new_collection`] does, then reads
    /// it to run it, without waiting.
    pub fn new_collection_loaded_later(
        &self,
        name: &str,
        canvas: &CanvasProfile,
        copy: bool,
    ) -> Pending<LoadedCollection> {
        let made = entries::new_collection(name, copy.then(|| self.collection_id.clone()), canvas);
        let shared = self.shared();
        self.later(move |data, lock, names| {
            let id = made(data, lock, names)?;
            load_collection(id, shared)(data)
        })
    }

    fn shared(&self) -> Option<Arc<Shared>> {
        self.saver.as_ref().map(|s| Arc::clone(&s.shared))
    }
}

/// Reading work that needs no writer lock: on the writer thread behind the saves, or
/// at once in a read-only session (no writer, so no saves to wait for).
pub(super) trait ReadTask<R>:
    FnOnce(&UserData) -> Result<R, SessionError> + Send + 'static
{
}

impl<R, F> ReadTask<R> for F where F: FnOnce(&UserData) -> Result<R, SessionError> + Send + 'static {}

pub(super) fn load_profile(id: EntryId) -> impl ReadTask<LoadedProfile> {
    move |data| {
        let entry = data.profiles().load(&id).map_err(SessionError::from)?;
        Ok(LoadedProfile {
            id,
            name: entry.name,
            document: entry.document,
        })
    }
}

/// Reads the collection `id` and signs it out where another collection holds the
/// same kept sign-in ([`sign_out_taken`]).
pub(super) fn load_collection(
    id: EntryId,
    shared: Option<Arc<Shared>>,
) -> impl ReadTask<LoadedCollection> {
    move |data| {
        let mut entry = data
            .scene_collections()
            .load(&id)
            .map_err(SessionError::from)?;
        sign_out_taken(data, &id, &mut entry.document, shared.as_deref());
        Ok(LoadedCollection {
            id,
            name: entry.name,
            collection: entry.document,
        })
    }
}

/// A kept sign-in another collection already holds stays with it; the loaded
/// collection `id` starts signed out there (ADR-0132 decision 10), and is saved so
/// through `shared`. Other collections are read only when it keeps a sign-in; the
/// caller has their saves written first.
pub(super) fn sign_out_taken(
    data: &UserData,
    id: &EntryId,
    collection: &mut SceneCollection,
    shared: Option<&Shared>,
) {
    if !crate::browser_profiles::keeps_any(collection) {
        return;
    }
    let taken = profile_ids_except(data, id);
    if crate::browser_profiles::renew(collection, &taken, false).unwrap_or(0) > 0
        && let Some(s) = shared
    {
        s.push(Job::Collection(id.clone(), collection.clone()));
    }
}

/// The kept-profile ids of every saved collection but `except`. A collection that
/// cannot be read adds none.
fn profile_ids_except(data: &UserData, except: &EntryId) -> BTreeSet<String> {
    let lib = data.scene_collections();
    super::listed(&lib)
        .into_iter()
        .filter(|(id, _)| id != except)
        .filter_map(|(id, _)| lib.load(&id).ok())
        .flat_map(|e| crate::browser_profiles::ids(&e.document))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::super::RECORDING;
    use super::super::later::tests::{block_on, dir, quiet};
    use super::super::tests::open;
    use super::*;
    use crate::record_profile::RecordProfile;

    #[test]
    fn a_profile_loaded_later_reads_the_saves_asked_for_before() {
        let d = dir("switch-profile");
        let (mut s, _) = open(&d, quiet());
        let first = s.profile_id.clone();
        let other = block_on(s.new_profile_loaded_later("Live", false)).unwrap();
        assert_eq!(other.name, "Live");
        let other_id = other.id().clone();
        s.use_profile(other);
        assert_eq!(s.profile_id, other_id);
        let mut rec = RecordProfile::new();
        rec.video_kbps = 23_456;
        // Queued, not yet written: the load runs after it on the writer thread.
        s.set_section(RECORDING, &rec).unwrap();
        let back = block_on(s.load_profile_later(&first)).unwrap();
        s.use_profile(back);
        let again = block_on(s.load_profile_later(&other_id)).unwrap();
        s.use_profile(again);
        let read: RecordProfile = s.section(RECORDING).unwrap().unwrap();
        assert_eq!(read.video_kbps, 23_456);
        assert_eq!(s.profile_name, "Live");
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_collection_loaded_later_is_the_saved_one() {
        let d = dir("switch-collection");
        let (s, _) = open(&d, quiet());
        let made =
            block_on(s.new_collection_loaded_later("Show", &CanvasProfile::default(), false))
                .unwrap();
        assert_eq!(made.name, "Show");
        let again = block_on(s.load_collection_later(&made.id)).unwrap();
        assert_eq!(again.name, "Show");
        assert_eq!(
            again.collection.sources().count(),
            made.collection.sources().count()
        );
        assert!(block_on(s.load_collection_later(&EntryId::parse("gone").unwrap())).is_err());
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_read_only_session_loads_at_once() {
        let d = dir("switch-read-only");
        let (s, _) = open(&d, quiet());
        let (other, _) = open(&d, quiet());
        assert!(!other.writable());
        let loaded = block_on(other.load_profile_later(&s.profile_id)).unwrap();
        assert_eq!(loaded.id(), &s.profile_id);
        assert!(block_on(other.load_collection_later(&s.collection_id)).is_ok());
        drop((s, other));
        let _ = std::fs::remove_dir_all(&d);
    }
}
