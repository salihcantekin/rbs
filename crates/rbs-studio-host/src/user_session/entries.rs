//! New, rename and delete of output profiles and scene collections on the writer
//! thread. The window awaits them ([`Pending`]); the sync forms in `user_session.rs`
//! run the same tasks and wait (control API, tests).

use rbs_user_data::{EntryId, OutputProfile, UserData, WriterLock};

use super::later::Pending;
use super::{Names, SessionError, UserSession, own_profiles, starter_collection};
use crate::canvas_profile::CanvasProfile;

/// Writer-thread work that answers `R`.
pub(super) trait Task<R>:
    FnOnce(&UserData, &WriterLock, &Names) -> Result<R, SessionError> + Send + 'static
{
}

impl<R, F> Task<R> for F where
    F: FnOnce(&UserData, &WriterLock, &Names) -> Result<R, SessionError> + Send + 'static
{
}

impl UserSession {
    /// [`UserSession::new_profile`] without waiting.
    pub fn new_profile_later(&self, name: &str, copy: bool) -> Pending<EntryId> {
        self.later(new_profile(name, copy.then(|| self.profile_id.clone())))
    }

    /// Renames the output profile in use without waiting; answers the new name, which
    /// the caller sets as `profile_name`.
    pub fn rename_profile_later(&self, name: &str) -> Pending<String> {
        self.later(rename(self.profile_id.clone(), name, true))
    }

    /// [`UserSession::delete_profile`] without waiting.
    pub fn delete_profile_later(&self, id: &EntryId) -> Pending<()> {
        if *id == self.profile_id {
            return Pending::ready(Err(SessionError::ProfileInUse));
        }
        self.later(delete_profile(id.clone()))
    }

    /// [`UserSession::new_collection`] without waiting.
    pub fn new_collection_later(
        &self,
        name: &str,
        canvas: &CanvasProfile,
        copy: bool,
    ) -> Pending<EntryId> {
        let from = copy.then(|| self.collection_id.clone());
        self.later(new_collection(name, from, canvas))
    }

    /// Renames the scene collection in use without waiting; answers the new name,
    /// which the caller sets as `collection_name`.
    pub fn rename_collection_later(&self, name: &str) -> Pending<String> {
        self.later(rename(self.collection_id.clone(), name, false))
    }

    /// [`UserSession::delete_collection`] without waiting.
    pub fn delete_collection_later(&self, id: &EntryId) -> Pending<()> {
        if *id == self.collection_id {
            return Pending::ready(Err(SessionError::CollectionInUse));
        }
        self.later(delete_collection(id.clone()))
    }

    /// Deletes the collection `old` a switch left behind, then renames the one in use
    /// to `name` when given; answers the rename's own outcome, once the delete ran.
    pub fn delete_left_collection_later(
        &self,
        old: &EntryId,
        name: Option<&str>,
    ) -> Pending<Option<Result<String, SessionError>>> {
        if *old == self.collection_id {
            return Pending::ready(Err(SessionError::CollectionInUse));
        }
        let delete = delete_collection(old.clone());
        let rename = name.map(|n| rename(self.collection_id.clone(), n, false));
        self.later(move |data, lock, names| {
            delete(data, lock, names)?;
            Ok(rename.map(|r| r(data, lock, names)))
        })
    }
}

/// A profile named `name` with default settings, or a copy of `from`; its id. The
/// stream key is not copied: it belongs to one profile.
pub(super) fn new_profile(name: &str, from: Option<EntryId>) -> impl Task<EntryId> {
    let name = name.to_owned();
    move |data, lock, _| {
        let lib = data.profiles();
        match from {
            Some(from) => lib.duplicate(lock, &from, &name),
            None => lib.create(lock, &name, &OutputProfile::new()),
        }
        .map_err(SessionError::from)
    }
}

/// Moves the profile `id` to `deleted/` (ADR-0051).
pub(super) fn delete_profile(id: EntryId) -> impl Task<()> {
    move |data, lock, _| {
        data.profiles()
            .delete(lock, &id)
            .map(|_| ())
            .map_err(SessionError::from)
    }
}

/// A collection named `name`, new (one canvas of `canvas` and one scene) or a copy of
/// `from` with profiles of its own; its id.
pub(super) fn new_collection(
    name: &str,
    from: Option<EntryId>,
    canvas: &CanvasProfile,
) -> impl Task<EntryId> {
    let name = name.to_owned();
    let starter = starter_collection(canvas);
    move |data, lock, _| {
        let lib = data.scene_collections();
        let copied = from.is_some();
        let id = match from {
            Some(from) => lib.duplicate(lock, &from, &name),
            None => lib.create(lock, &name, &starter),
        }
        .map_err(SessionError::from)?;
        if copied {
            own_profiles(data, lock, &id)?;
        }
        Ok(id)
    }
}

/// Moves the collection `id` to `deleted/`, with its OBS import report (ADR-0131
/// decision 11).
pub(super) fn delete_collection(id: EntryId) -> impl Task<()> {
    move |data, lock, _| {
        data.scene_collections()
            .delete(lock, &id)
            .map_err(SessionError::from)?;
        let _ = std::fs::remove_file(crate::obs_import::report_path(data.root(), &id));
        let _ = data.delete_view_state(lock, &id);
        Ok(())
    }
}

/// Renames the profile (`profile`) or collection `id` to `name`, trimmed; that name. On
/// the writer thread, saves asked for before keep the old name and later ones the new.
pub(super) fn rename(id: EntryId, name: &str, profile: bool) -> impl Task<String> {
    let name = name.trim().to_owned();
    move |data, lock, names| {
        let done = if profile {
            data.profiles().rename(lock, &id, &name)
        } else {
            data.scene_collections().rename(lock, &id, &name)
        };
        done.map_err(SessionError::from)?;
        let map = if profile {
            &names.profiles
        } else {
            &names.collections
        };
        Names::set(map, &id, &name);
        Ok(name)
    }
}
