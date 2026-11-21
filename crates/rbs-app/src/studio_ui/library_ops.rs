//! The Output profile and Scene collection menus' switch, New, Duplicate, Rename and
//! Delete. The writer thread does the file work, and reads the entry a switch uses;
//! the window awaits it ([`EntryWrite`]) and finishes the change on the UI thread, so
//! the UI thread never waits on the disk (docs/architecture/threads.md).

use std::future::Future;
use std::pin::Pin;

use rbs_app::user_session::{LoadedCollection, LoadedProfile, SessionError};

use super::*;

/// What finishes an entry change on the UI thread, given the function that closes the
/// preview pictures before a collection switch; it may leave a next step for the
/// writer thread, awaited the same way.
pub type Finish = Box<dyn FnOnce(&mut StudioUi, fn()) -> Result<Option<EntryWrite>, String>>;

/// An entry change the writer thread is doing; [`EntryWrite::written`] answers what
/// finishes it.
#[must_use = "the change finishes only when awaited"]
pub struct EntryWrite(Pin<Box<dyn Future<Output = Finish>>>);

impl EntryWrite {
    /// Once `work` ran, `then` finishes with its result; a failed write reads the lists
    /// again and answers its error.
    fn after<R: 'static>(
        work: impl Future<Output = Result<R, SessionError>> + 'static,
        then: impl FnOnce(&mut StudioUi, R, fn()) -> Result<(), String> + 'static,
    ) -> Self {
        Self::step(work, false, move |ui, r, close| {
            then(ui, r, close).map(|()| None)
        })
    }

    /// [`EntryWrite::after`] for the read a switch waits on: `StudioUi::wait.loading` is
    /// set until it finishes, and `then` may leave a next step.
    fn loaded<R: 'static>(
        ui: &mut StudioUi,
        work: impl Future<Output = Result<R, SessionError>> + 'static,
        then: impl FnOnce(&mut StudioUi, R, fn()) -> Result<Option<EntryWrite>, String> + 'static,
    ) -> Self {
        ui.wait.loading = true;
        Self::step(work, true, then)
    }

    fn step<R: 'static>(
        work: impl Future<Output = Result<R, SessionError>> + 'static,
        loading: bool,
        then: impl FnOnce(&mut StudioUi, R, fn()) -> Result<Option<EntryWrite>, String> + 'static,
    ) -> Self {
        Self(Box::pin(async move {
            let done = work.await;
            Box::new(move |ui: &mut StudioUi, close: fn()| {
                if loading {
                    ui.wait.loading = false;
                }
                match done {
                    Ok(r) => then(ui, r, close),
                    Err(e) => {
                        ui.refresh_library();
                        Err(e.to_string())
                    }
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
    /// Output profile menu: uses the profile `id` (refused while live). `None` when it
    /// is the one in use.
    pub fn switch_profile(&mut self, id: &str) -> Result<Option<EntryWrite>, String> {
        let id = EntryId::parse(id).ok_or("that output profile is gone")?;
        if id == self.user.profile_id {
            return Ok(None);
        }
        self.can_switch_profile("switching output profiles")?;
        let load = self.user.load_profile_later(&id);
        Ok(Some(EntryWrite::loaded(self, load, |ui, loaded, _| {
            ui.use_profile(loaded, "switching output profiles")
                .map(|()| None)
        })))
    }

    /// Why an output profile switch cannot start now: live, or another switch still
    /// reads its entry.
    fn can_switch_profile(&self, doing: &str) -> Result<(), String> {
        self.idle_for(doing)?;
        if self.wait.loading {
            return Err("The output profile is still loading.".into());
        }
        Ok(())
    }

    /// Uses the profile the writer thread read, unless the studio went live since.
    /// The studio gets the profile's settings and key account before the list that
    /// shows it in use.
    fn use_profile(&mut self, loaded: LoadedProfile, doing: &str) -> Result<(), String> {
        let result = self.idle_for(doing);
        if result.is_ok() {
            self.user.use_profile(loaded);
            self.profile_loaded();
        }
        self.refresh_library();
        result
    }

    /// Output profile menu: makes a profile named `name`, with default settings or a
    /// copy of the one in use (`copy`), and uses it.
    pub fn new_profile(&mut self, name: &str, copy: bool) -> Result<EntryWrite, String> {
        self.can_switch_profile("switching output profiles")?;
        let made = self.user.new_profile_loaded_later(name, copy);
        Ok(EntryWrite::loaded(self, made, |ui, loaded, _| {
            ui.use_profile(loaded, "switching output profiles")
                .map(|()| None)
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
        if id != self.user.profile_id {
            return Ok(self.delete_unused_profile(id));
        }
        let other = self
            .user
            .profiles()
            .into_iter()
            .find(|(other, _)| *other != id)
            .ok_or("The only output profile cannot be deleted. Make another first.")?;
        const DOING: &str = "deleting the output profile in use";
        self.can_switch_profile(DOING)?;
        let load = self.user.load_profile_later(&other.0);
        // The delete is queued once the other profile is in use, so no save of the
        // deleted one can follow it.
        Ok(EntryWrite::loaded(self, load, move |ui, loaded, _| {
            ui.use_profile(loaded, DOING)?;
            Ok(Some(ui.delete_unused_profile(id)))
        }))
    }

    /// Deletes the profile `id`, which is not the one in use, and its stream keys.
    fn delete_unused_profile(&mut self, id: EntryId) -> EntryWrite {
        let deleted = self.user.delete_profile_later(&id);
        EntryWrite::after(deleted, move |ui, (), _| {
            ui.refresh_library();
            // ADR-0051 decision 5: the keys go with the profile.
            let account = stream_key::account(id.as_str());
            ui.send(Command::RemoveStreamKey(stream_key::vertical_account(
                &account,
            )));
            ui.send(Command::RemoveStreamKey(account));
            Ok(())
        })
    }

    /// Scene collection menu: runs the collection `id` (refused while live; the studio
    /// restarts, so the preview connects again). `None` when it is the one in use.
    pub fn switch_collection(&mut self, id: &str) -> Result<Option<EntryWrite>, String> {
        let id = EntryId::parse(id).ok_or("that scene collection is gone")?;
        if id == self.user.collection_id {
            return Ok(None);
        }
        self.can_switch_collection()?;
        let load = self.user.load_collection_later(&id);
        Ok(Some(EntryWrite::loaded(self, load, |ui, loaded, close| {
            ui.run_loaded(loaded, None, false, close).map(|()| None)
        })))
    }

    /// Asks the studio to run the collection the writer thread read, unless a switch
    /// became impossible since; `old` is deleted once the switch is answered.
    fn run_loaded(
        &mut self,
        loaded: LoadedCollection,
        old: Option<EntryId>,
        prefer_default_name: bool,
        close_preview: fn(),
    ) -> Result<(), String> {
        if let Err(why) = self.can_switch_collection() {
            self.refresh_library();
            return Err(why);
        }
        let LoadedCollection {
            id,
            name,
            collection,
        } = loaded;
        self.ask_switch(
            id,
            name,
            collection,
            old,
            prefer_default_name,
            close_preview,
        );
        // After the ask: a control API switch is answered once the studio answers.
        self.refresh_library();
        Ok(())
    }

    /// Scene collection menu: makes a collection named `name`, new (one canvas the
    /// size of the one in use, one scene) or a copy of the one in use (`copy`), and
    /// runs it.
    pub fn new_collection(&mut self, name: &str, copy: bool) -> Result<EntryWrite, String> {
        self.can_switch_collection()?;
        let made = self
            .user
            .new_collection_loaded_later(name, &self.canvas, copy);
        Ok(EntryWrite::loaded(self, made, |ui, loaded, close| {
            ui.run_loaded(loaded, None, false, close).map(|()| None)
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
    /// first runs another (refused while live), and the delete follows the studio's
    /// answer. The only scene collection is replaced by a new [`DEFAULT_NAME`]
    /// collection with one empty scene.
    pub fn delete_collection(&mut self, id: &str) -> Result<EntryWrite, String> {
        let id = EntryId::parse(id).ok_or("that scene collection is gone")?;
        if id != self.user.collection_id {
            let deleted = self.user.delete_collection_later(&id);
            return Ok(EntryWrite::after(deleted, |ui, (), _| {
                ui.refresh_library();
                Ok(())
            }));
        }
        self.can_switch_collection()?;
        let other = self
            .user
            .collections()
            .into_iter()
            .find(|(other, _)| *other != id);
        let (load, prefer_default_name) = match other {
            Some((other, _)) => (self.user.load_collection_later(&other), false),
            None => {
                let name = free_collection_name(&self.user, DEFAULT_NAME);
                let prefer = !name.eq_ignore_ascii_case(DEFAULT_NAME);
                let made = self
                    .user
                    .new_collection_loaded_later(&name, &self.canvas, false);
                (made, prefer)
            }
        };
        Ok(EntryWrite::loaded(self, load, move |ui, loaded, close| {
            ui.run_loaded(loaded, Some(id), prefer_default_name, close)
                .map(|()| None)
        }))
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
    fn left_deleted(
        work: impl Future<Output = Result<Option<Result<String, SessionError>>, SessionError>> + 'static,
    ) -> Self {
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
                told.map(|()| None)
            }) as Finish
        }))
    }
}

/// Runs `write` and every step it leaves to their end on this thread, as the window's
/// event loop would (tests).
#[cfg(test)]
pub(super) fn finish(ui: &mut StudioUi, mut write: Option<EntryWrite>) -> Result<(), String> {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    while let Some(w) = write.take() {
        let mut f = std::pin::pin!(w.written());
        let done = loop {
            if let Poll::Ready(done) = f.as_mut().poll(&mut cx) {
                break done;
            }
            std::thread::park();
        };
        write = done(ui, || {})?;
    }
    Ok(())
}
