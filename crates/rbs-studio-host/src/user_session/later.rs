//! Writer-thread work the UI thread does not wait for (docs/architecture/threads.md,
//! stability plan WP-I follow-up): the job is queued behind every save asked for so
//! far, as [`UserSession::run`] queues it, and its result arrives as a [`Pending`]
//! future, which the window awaits under `entry::spawn_local`.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rbs_import_obs::ImportPlan;
use rbs_user_data::{EntryId, UserData, WriterLock};

use super::{Job, Names, SessionError, UserSession, own_profiles, read_only_note, scrub_profile};

/// The result of writer-thread work, once it ran; `Err` with a note when changes are
/// not saved (read-only) or the writer stopped first.
#[must_use = "the result arrives only when awaited"]
pub struct Pending<R> {
    state: Arc<Mutex<State<R>>>,
}

struct State<R> {
    value: Option<Result<R, SessionError>>,
    waker: Option<Waker>,
}

/// The writer thread's end of a [`Pending`]: filled once; dropped unfilled (the writer
/// stopped before the job ran) it reports that.
struct Slot<R> {
    state: Option<Arc<Mutex<State<R>>>>,
}

impl<R> Slot<R> {
    fn fill(mut self, value: Result<R, SessionError>) {
        if let Some(state) = self.state.take() {
            put(&state, value);
        }
    }
}

impl<R> Drop for Slot<R> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            put(&state, Err(SessionError::WriterStopped));
        }
    }
}

fn put<R>(state: &Mutex<State<R>>, value: Result<R, SessionError>) {
    let waker = match state.lock() {
        Ok(mut s) => {
            s.value = Some(value);
            s.waker.take()
        }
        Err(_) => None,
    };
    if let Some(w) = waker {
        w.wake();
    }
}

impl<R> Pending<R> {
    fn new() -> (Self, Slot<R>) {
        let state = Arc::new(Mutex::new(State {
            value: None,
            waker: None,
        }));
        let slot = Slot {
            state: Some(Arc::clone(&state)),
        };
        (Self { state }, slot)
    }

    pub(super) fn ready(value: Result<R, SessionError>) -> Self {
        let (pending, slot) = Self::new();
        slot.fill(value);
        pending
    }
}

impl<R> Future for Pending<R> {
    type Output = Result<R, SessionError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Ok(mut s) = self.state.lock() else {
            return Poll::Ready(Err(SessionError::WriterStopped));
        };
        match s.value.take() {
            Some(v) => Poll::Ready(v),
            None => {
                s.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl UserSession {
    /// Queues `task` on the writer thread after every save asked for so far, as
    /// [`UserSession::run`] does, without waiting for it.
    pub(super) fn later<R: Send + 'static>(
        &self,
        task: impl FnOnce(&UserData, &WriterLock, &Names) -> Result<R, SessionError> + Send + 'static,
    ) -> Pending<R> {
        let Some(saver) = self.saver.as_ref() else {
            return Pending::ready(Err(read_only_note()));
        };
        self.revision.fetch_add(1, Ordering::Relaxed);
        let (pending, slot) = Pending::new();
        saver
            .shared
            .push(Job::Run(Box::new(move |data, lock, names| {
                slot.fill(task(data, lock, names));
            })));
        pending
    }

    /// [`UserSession::export_profile`] without waiting: the file is written on the
    /// writer thread after the saves asked for before.
    pub fn export_profile_later(&self, to: &Path) -> Pending<()> {
        let (id, to) = (self.profile_id.clone(), to.to_owned());
        self.later(move |data, _, _| {
            data.profiles()
                .export_with(&id, &to, scrub_profile)
                .map_err(SessionError::from)
        })
    }

    /// [`UserSession::export_collection`] without waiting.
    pub fn export_collection_later(&self, to: &Path) -> Pending<()> {
        let (id, to) = (self.collection_id.clone(), to.to_owned());
        self.later(move |data, _, _| {
            data.scene_collections()
                // A kept sign-in belongs to this computer (ADR-0132 decision 10).
                .export_with(&id, &to, crate::browser_profiles::strip)
                .map_err(SessionError::from)
        })
    }

    /// [`UserSession::import_profile`] without waiting.
    pub fn import_profile_later(&self, from: &Path) -> Pending<String> {
        self.later(import_profile(from.to_owned()))
    }

    /// [`UserSession::import_collection`] without waiting.
    pub fn import_collection_later(&self, from: &Path) -> Pending<String> {
        self.later(import_collection(from.to_owned()))
    }

    /// Adds the theme in the file `from` (a clashing name gets a number); its id once
    /// written.
    pub fn import_theme(&self, from: &Path) -> Pending<EntryId> {
        let from = from.to_owned();
        self.later(move |data, lock, _| {
            data.themes()
                .import(lock, &from)
                .map(|(id, _)| id)
                .map_err(SessionError::from)
        })
    }

    /// Writes a theme to `to` (one file; nothing to scrub) after the saves asked for
    /// before.
    pub fn export_theme(&self, id: &EntryId, to: &Path) -> Pending<()> {
        let (id, to) = (id.clone(), to.to_owned());
        self.later(move |data, _, _| data.themes().export(&id, &to).map_err(SessionError::from))
    }

    /// [`UserSession::import_obs`] without waiting.
    pub fn import_obs_later(&self, plan: ImportPlan) -> Pending<ObsWritten> {
        self.later(import_obs(plan))
    }
}

/// What an OBS import wrote, and the stream key to save under the new profile's
/// account, if any.
pub type ObsWritten = (
    crate::obs_import::Written,
    Option<(String, rbs_transport::Secret)>,
);

/// Writes an OBS import plan (ADR-0131 decision 9).
pub(super) fn import_obs(
    plan: ImportPlan,
) -> impl FnOnce(&UserData, &WriterLock, &Names) -> Result<ObsWritten, SessionError> + Send + 'static
{
    move |data, lock, _| {
        let key = std::cell::RefCell::new(None);
        let written = crate::obs_import::write(
            data,
            lock,
            &plan,
            &|account, secret| {
                *key.borrow_mut() = Some((account.to_owned(), secret));
                Ok(())
            },
            rbs_ui::shortcut::Platform::current(),
            &|p| p.is_dir(),
        );
        written
            .map(|w| (w, key.into_inner()))
            .map_err(SessionError::from)
    }
}

/// Adds the output profile in the file `from`; its name.
pub(super) fn import_profile(
    from: PathBuf,
) -> impl FnOnce(&UserData, &WriterLock, &Names) -> Result<String, SessionError> + Send + 'static {
    move |data, lock, _| {
        data.profiles()
            .import(lock, &from)
            .map(|(_, name)| name)
            .map_err(SessionError::from)
    }
}

/// Adds the scene collection in the file `from`, with profiles of its own; its name.
pub(super) fn import_collection(
    from: PathBuf,
) -> impl FnOnce(&UserData, &WriterLock, &Names) -> Result<String, SessionError> + Send + 'static {
    move |data, lock, _| {
        let (id, name) = data
            .scene_collections()
            .import(lock, &from)
            .map_err(SessionError::from)?;
        own_profiles(data, lock, &id)?;
        Ok(name)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
pub(super) mod tests {
    use std::sync::Arc;
    use std::task::Wake;
    use std::thread::Thread;

    use super::super::{OnError, RECORDING, open};
    use super::*;
    use crate::canvas_profile::CanvasProfile;
    use crate::record_profile::RecordProfile;

    /// Runs `f` to its end on this thread: parks until the writer thread wakes it.
    pub(crate) fn block_on<F: Future>(f: F) -> F::Output {
        struct Unpark(Thread);
        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut f = std::pin::pin!(f);
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::park();
        }
    }

    pub(crate) fn dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("rbs-session-later-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub(crate) fn quiet() -> OnError {
        Arc::new(|_| {})
    }

    #[test]
    fn an_export_and_import_later_follow_the_saves_asked_for_before() {
        let d = dir("export");
        let (mut s, _) = open(&d, quiet());
        let mut rec = RecordProfile::new();
        rec.video_kbps = 12_345;
        // Queued, not yet written: the export runs after it on the writer thread.
        s.set_section(RECORDING, &rec).unwrap();
        let file = d.join("shared.toml");
        block_on(s.export_profile_later(&file)).unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains("12345"));
        assert_eq!(
            block_on(s.import_profile_later(&file)).unwrap(),
            "Default 2"
        );
        assert!(s.profiles().iter().any(|(_, n)| n == "Default 2"));
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_read_only_session_answers_at_once() {
        let d = dir("read-only");
        let (s, _) = open(&d, quiet());
        let (other, _) = open(&d, quiet());
        assert!(!other.writable());
        let file = d.join("x.toml");
        assert!(matches!(
            block_on(other.export_profile_later(&file)),
            Err(SessionError::ReadOnly)
        ));
        assert!(!file.exists());
        drop((s, other));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_job_the_writer_never_ran_reports_that_it_stopped() {
        let (pending, slot) = Pending::<()>::new();
        drop(slot);
        assert!(matches!(
            block_on(pending),
            Err(SessionError::WriterStopped)
        ));
    }

    #[test]
    fn entry_writes_later_answer_once_written() {
        let d = dir("entries");
        let (mut s, _) = open(&d, quiet());
        let id =
            block_on(s.new_collection_later("Show", &CanvasProfile::default(), false)).unwrap();
        assert!(s.collections().iter().any(|(c, n)| *c == id && n == "Show"));
        assert!(matches!(
            block_on(s.delete_collection_later(&s.collection_id.clone())),
            Err(SessionError::CollectionInUse)
        ));
        // The collection a switch left goes, then the one in use takes the name.
        let left = s.collection_id.clone();
        s.collection_switched(&id, "Show");
        let renamed = block_on(s.delete_left_collection_later(&left, Some("Default"))).unwrap();
        assert_eq!(renamed.unwrap().unwrap(), "Default");
        assert!(s.collections().iter().all(|(c, _)| *c != left));
        assert_eq!(
            block_on(s.rename_profile_later("  Live  ")).unwrap(),
            "Live"
        );
        let made = block_on(s.new_profile_later("Spare", false)).unwrap();
        block_on(s.delete_profile_later(&made)).unwrap();
        assert!(s.profiles().iter().all(|(p, _)| *p != made));
    }
}
