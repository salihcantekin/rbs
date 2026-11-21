//! The UI thread's entry points, each a panic boundary (ADR-0055 decision 2).
//!
//! Everything the toolkit calls back is installed through these functions, so a panic
//! in our code never unwinds into Slint, winit or OS frames (on macOS that aborts the
//! process). `clippy.toml` bans the Slint functions wrapped here everywhere else in this
//! crate. Callbacks of the generated `.slint` globals go through `Ctx::update`, which
//! runs its change under [`guard`] as well.
//!
//! A boundary cannot catch a panic raised below a nested OS event loop (the Export crash
//! of PR #143): AppKit frames lie between it and us. Decision 6, no nested event loops,
//! is what prevents those.
#![allow(clippy::disallowed_methods)] // The one place that may call them.

use std::future::Future;
use std::panic::Location;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

/// Runs `f`; a panic is shown in the error bar (rate-limited) and `fallback` is returned.
#[track_caller]
pub fn guard_or<R>(fallback: impl FnOnce() -> R, f: impl FnOnce() -> R) -> R {
    guard_at(Location::caller(), fallback, f)
}

/// [`guard_or`] for an entry installed at `at`.
fn guard_at<R>(
    at: &'static Location<'static>,
    fallback: impl FnOnce() -> R,
    f: impl FnOnce() -> R,
) -> R {
    let _entry = rbs_app::ui_thread::enter(at);
    match rbs_crash::contain(f) {
        Ok(r) => r,
        Err(_) => {
            crate::show_caught();
            fallback()
        }
    }
}

/// Runs `f`; a panic is shown in the error bar (rate-limited).
#[track_caller]
pub fn guard(f: impl FnOnce()) {
    guard_at(Location::caller(), || (), f);
}

/// [`guard`] for an entry installed at `at`.
fn guard_from(at: &'static Location<'static>, f: impl FnOnce()) {
    guard_at(at, || (), f);
}

/// `slint::Timer::single_shot` with the body guarded.
#[track_caller]
pub fn single_shot(after: Duration, f: impl FnOnce() + 'static) {
    let at = Location::caller();
    slint::Timer::single_shot(after, move || guard_from(at, f));
}

/// `slint::Timer::start` with the body guarded.
#[track_caller]
pub fn start(
    timer: &slint::Timer,
    mode: slint::TimerMode,
    interval: Duration,
    mut f: impl FnMut() + 'static,
) {
    let at = Location::caller();
    timer.start(mode, interval, move || guard_from(at, &mut f));
}

/// `slint::invoke_from_event_loop` with the body guarded. Callable from any thread.
#[track_caller]
pub fn invoke(f: impl FnOnce() + Send + 'static) -> Result<(), slint::EventLoopError> {
    let at = Location::caller();
    slint::invoke_from_event_loop(move || guard_from(at, f))
}

/// Hands `value` to the UI thread through `slot`: it replaces a value still waiting
/// there, and a call is posted only when none is pending (stability plan WP-J). The
/// posted call runs `show` with the newest value; a replaced value is dropped here, on
/// the calling thread. Err when the event loop is gone.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))] // Frame threads.
#[track_caller]
pub fn invoke_latest<T: Send + 'static>(
    slot: &std::sync::Arc<rbs_app::latest::Latest<T>>,
    value: T,
    show: impl FnOnce(T) + Send + 'static,
) -> Result<(), slint::EventLoopError> {
    let (post, replaced) = slot.put(value);
    drop(replaced);
    if !post {
        return Ok(());
    }
    let at = Location::caller();
    let taken = std::sync::Arc::clone(slot);
    let posted = slint::invoke_from_event_loop(move || {
        guard_from(at, move || {
            if let Some(value) = taken.take() {
                show(value);
            }
        })
    });
    if posted.is_err() {
        slot.unpost();
    }
    posted
}

/// Ends the UI event loop. Slint posts the quit as an event that the loop takes on a
/// clean stack, also when this runs inside a platform close message (ADR-0156
/// decision 2). Called once the shutdown's last step has finished.
pub fn quit_loop() {
    let _ = slint::quit_event_loop();
}

/// `slint::spawn_local` with every poll of the task guarded: a panic ends the task.
#[track_caller]
pub fn spawn_local(
    task: impl Future<Output = ()> + 'static,
) -> Result<slint::JoinHandle<()>, slint::EventLoopError> {
    slint::spawn_local(Guarded(Some(Box::pin(task)), Location::caller()))
}

/// Awaits `work` (writer-thread work, `user_session::Pending`) under [`spawn_local`]
/// and hands its result to `done`; when no task can start, `done` gets that error at
/// once. The UI thread never waits on the writer (docs/architecture/threads.md).
#[track_caller]
pub fn when_done<R: 'static>(
    work: impl Future<Output = Result<R, String>> + 'static,
    done: impl FnOnce(Result<R, String>) + 'static,
) {
    let done = std::rc::Rc::new(std::cell::Cell::new(Some(done)));
    let then = std::rc::Rc::clone(&done);
    let task = spawn_local(async move {
        let result = work.await;
        if let Some(f) = then.take() {
            f(result);
        }
    });
    if let (Err(e), Some(f)) = (task, done.take()) {
        f(Err(e.to_string()));
    }
}

/// `Window::on_close_requested` with the handler guarded; a panic keeps the window.
#[track_caller]
pub fn on_close_requested(
    window: &slint::Window,
    mut f: impl FnMut() -> slint::CloseRequestResponse + 'static,
) {
    let at = Location::caller();
    window.on_close_requested(move || {
        guard_at(at, || slint::CloseRequestResponse::KeepWindowShown, &mut f)
    });
}

/// `Window::set_rendering_notifier` with the notifier guarded.
#[allow(dead_code)] // Used on macOS and Windows only (the preview layer).
#[track_caller]
pub fn set_rendering_notifier(
    window: &slint::Window,
    mut f: impl FnMut(slint::RenderingState, &slint::GraphicsAPI<'_>) + 'static,
) -> Result<(), slint::SetRenderingNotifierError> {
    let at = Location::caller();
    window.set_rendering_notifier(move |state, api| guard_from(at, || f(state, api)))
}

/// A task whose polls, and whose drop, run under the panic boundary. After a panic the
/// task is dropped inside the boundary too, so nothing of it unwinds into Slint. The
/// place is where the task was spawned (the stall watch's entry).
struct Guarded(
    Option<Pin<Box<dyn Future<Output = ()>>>>,
    &'static Location<'static>,
);

impl Future for Guarded {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let at = self.1;
        let Some(task) = self.0.as_mut() else {
            return Poll::Ready(());
        };
        let entry = rbs_app::ui_thread::enter(at);
        let polled = rbs_crash::contain(|| task.as_mut().poll(cx));
        drop(entry);
        match polled {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(())) => {
                drop_guarded(self.0.take(), at);
                Poll::Ready(())
            }
            Err(_) => {
                crate::show_caught();
                drop_guarded(self.0.take(), at);
                Poll::Ready(())
            }
        }
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        drop_guarded(self.0.take(), self.1);
    }
}

fn drop_guarded(task: Option<Pin<Box<dyn Future<Output = ()>>>>, at: &'static Location<'static>) {
    if let Some(task) = task {
        guard_from(at, move || drop(task));
    }
}
