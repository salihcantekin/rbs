//! The Output profile and Scene collection menus' New, Duplicate, Rename and Delete.
//! The writer thread does the file work; the window awaits it ([`EntryWrite`]) and
//! finishes the change on the UI thread, so the UI thread never waits on the disk
//! (docs/architecture/threads.md).

use std::future::Future;
use std::pin::Pin;

use rbs_app::user_session::{Pending, SessionError};

use super::*;

/// What finishes an entry change on the UI thread, given the function that closes the
/// preview pictures before a collection switch.
pub type Finish = Box<dyn FnOnce(&mut StudioUi, fn()) -> Result<(), String>>;

/// An entry change the writer thread is doing; [`EntryWrite::written`] answers what
/// finishes it.
#[must_use = "the change finishes only when awaited"]
pub struct EntryWrite(Pin<Box<dyn Future<Output = Finish>>>);

impl EntryWrite {
    /// Once `work` ran, `then` finishes with its result; a failed write reads the lists
    /// again and answers its error.
    fn after<R: 'static>(
        work: Pending<R>,
        then: impl FnOnce(&mut StudioUi, R, fn()) -> Result<(), String> + 'static,
    ) -> Self {
        Self(Box::pin(async move {
            let done = work.await;
            Box::new(move |ui: &mut StudioUi, close: fn()| match done {
                Ok(r) => then(ui, r, close),
                Err(e) => {
                    ui.refresh_library();
                    Err(e.to_string())
                }
            }) as Finish
        }))
    }

    /// Waits for the writer thread.
    pub async fn written(self) -> Finish {
        self.0.await
    }
}

impl StudioUi {
    /// Output profile menu: makes a profile named `name`, with default settings or a
    /// copy of the one in use (`copy`), and uses it.
    pub fn new_profile(&mut self, name: &str, copy: bool) -> Result<EntryWrite, String> {
        self.idle_for("switching output profiles")?;
        let made = self.user.new_profile_later(name, copy);
        Ok(EntryWrite::after(made, |ui, id, _| {
            let result = ui
                .idle_for("switching output profiles")
                .and_then(|()| ui.user.switch_profile(&id).map_err(String::from));
            if result.is_ok() {
                ui.profile_loaded();
            }
            ui.refresh_library();
            result
        }))
    }

    /// Output profile menu: renames the profile in use.
    pub fn rename_profile(&mut self, name: &str) -> EntryWrite {
        EntryWrite::after(self.user.rename_profile_later(name), |ui, name, _| {
            ui.user.profile_name = name;
            ui.refresh_library();
            Ok(())
        })
    }

    /// Output profile menu: deletes the profile `id` and its stream key. Deleting the
    /// one in use first switches to another (refused while live, and for the only one).
    pub fn delete_profile(&mut self, id: &str) -> Result<EntryWrite, String> {
        let id = EntryId::parse(id).ok_or("that output profile is gone")?;
        if id == self.user.profile_id {
            let other = self
                .user
                .profiles()
                .into_iter()
                .find(|(other, _)| *other != id)
                .ok_or("The only output profile cannot be deleted. Make another first.")?;
            self.idle_for("deleting the output profile in use")?;
            self.user.switch_profile(&other.0)?;
            self.profile_loaded();
        }
        let deleted = self.user.delete_profile_later(&id);
        Ok(EntryWrite::after(deleted, move |ui, (), _| {
            ui.refresh_library();
            // ADR-0051 decision 5: the keys go with the profile.
            let account = stream_key::account(id.as_str());
            ui.send(Command::RemoveStreamKey(stream_key::vertical_account(
                &account,
            )));
            ui.send(Command::RemoveStreamKey(account));
            Ok(())
        }))
    }

    /// Scene collection menu: makes a collection named `name`, new (one canvas the
    /// size of the one in use, one scene) or a copy of the one in use (`copy`), and
    /// runs it.
    pub fn new_collection(&mut self, name: &str, copy: bool) -> Result<EntryWrite, String> {
        self.can_switch_collection()?;
        let made = self.user.new_collection_later(name, &self.canvas, copy);
        Ok(EntryWrite::after(made, |ui, id, close_preview| {
            ui.refresh_library();
            ui.can_switch_collection()?;
            let (name, collection) = ui.user.load_collection(&id)?;
            ui.ask_switch(id, name, collection, None, false, close_preview);
            Ok(())
        }))
    }

    /// Scene collection menu: renames the collection in use.
    pub fn rename_collection(&mut self, name: &str) -> EntryWrite {
        EntryWrite::after(self.user.rename_collection_later(name), |ui, name, _| {
            ui.user.collection_name = name;
            ui.refresh_library();
            Ok(())
        })
    }

    /// Scene collection menu: deletes the collection `id`. Deleting the one in use
    /// first runs another (refused while live). The only scene collection is replaced
    /// by a new [`DEFAULT_NAME`] collection with one empty scene. `None`: nothing is
    /// left for the writer thread until the switch is answered.
    pub fn delete_collection(
        &mut self,
        id: &str,
        close_preview: fn(),
    ) -> Result<Option<EntryWrite>, String> {
        let id = EntryId::parse(id).ok_or("that scene collection is gone")?;
        if id != self.user.collection_id {
            let deleted = self.user.delete_collection_later(&id);
            return Ok(Some(EntryWrite::after(deleted, |ui, (), _| {
                ui.refresh_library();
                Ok(())
            })));
        }
        self.can_switch_collection()?;
        let other = self
            .user
            .collections()
            .into_iter()
            .find(|(other, _)| *other != id);
        let Some((other, _)) = other else {
            let name = free_collection_name(&self.user, DEFAULT_NAME);
            let prefer_default_name = !name.eq_ignore_ascii_case(DEFAULT_NAME);
            let made = self.user.new_collection_later(&name, &self.canvas, false);
            return Ok(Some(EntryWrite::after(
                made,
                move |ui, other, close_preview| {
                    ui.refresh_library();
                    ui.can_switch_collection()?;
                    ui.switch_leaving(other, id, prefer_default_name, close_preview)
                },
            )));
        };
        self.switch_leaving(other, id, false, close_preview)?;
        Ok(None)
    }

    /// Runs the collection `other`; `old` is deleted once the switch is answered.
    fn switch_leaving(
        &mut self,
        other: EntryId,
        old: EntryId,
        prefer_default_name: bool,
        close_preview: fn(),
    ) -> Result<(), String> {
        let (name, collection) = self.user.load_collection(&other)?;
        self.ask_switch(
            other,
            name,
            collection,
            Some(old),
            prefer_default_name,
            close_preview,
        );
        Ok(())
    }

    /// The studio answered a collection switch: what to tell the user goes in `out`.
    pub(super) fn collection_switched(
        &mut self,
        seq: u64,
        result: Result<(), String>,
        out: &mut Applied,
    ) {
        let Some(s) = self.switching.take_if(|s| s.seq == seq) else {
            return;
        };
        if let Err(why) = result {
            out.error = Some(why);
            self.refresh_library();
            return;
        }
        self.user.collection_switched(&s.id, &s.name);
        self.selected = -1;
        if let Some(old) = s.delete_after {
            // The last collection was replaced: once it is gone, prefer the name
            // "Default" for the empty starter that took its place.
            let rename = (s.prefer_default_name
                && !self.user.collection_name.eq_ignore_ascii_case(DEFAULT_NAME))
            .then_some(DEFAULT_NAME);
            let work = self.user.delete_left_collection_later(&old, rename);
            out.written = Some(EntryWrite::left_deleted(work));
        }
        self.refresh_library();
    }
}

impl EntryWrite {
    /// The collection a switch left is deleted, and the one in use maybe renamed.
    fn left_deleted(work: Pending<Option<Result<String, SessionError>>>) -> Self {
        Self(Box::pin(async move {
            let done = work.await;
            Box::new(move |ui: &mut StudioUi, _: fn()| {
                let told = match done {
                    Err(e) => Err(format!("The scene collection was not deleted: {e}")),
                    Ok(None) => Ok(()),
                    Ok(Some(Ok(name))) => {
                        ui.user.collection_name = name;
                        Ok(())
                    }
                    Ok(Some(Err(e))) => Err(format!(
                        "The new scene collection could not be named {DEFAULT_NAME}: {e}"
                    )),
                };
                ui.refresh_library();
                told
            }) as Finish
        }))
    }
}
