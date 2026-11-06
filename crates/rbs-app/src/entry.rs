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
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

/// Runs `f`; a panic is shown in the error bar (rate-limited) and `fallback` is returned.
pub fn guard_or<R>(fallback: impl FnOnce() -> R, f: impl FnOnce() -> R) -> R {
    match rbs_crash::contain(f) {
        Ok(r) => r,
        Err(_) => {
            crate::show_caught();
            fallback()
        }
    }
}

/// Runs `f`; a panic is shown in the error bar (rate-limited).
pub fn guard(f: impl FnOnce()) {
    guard_or(|| (), f);
}

/// `slint::Timer::single_shot` with the body guarded.
pub fn single_shot(after: Duration, f: impl FnOnce() + 'static) {
    slint::Timer::single_shot(after, move || guard(f));
}

/// `slint::Timer::start` with the body guarded.
pub fn start(
    timer: &slint::Timer,
    mode: slint::TimerMode,
    interval: Duration,
    mut f: impl FnMut() + 'static,
) {
    timer.start(mode, interval, move || guard(&mut f));
}

/// `slint::invoke_from_event_loop` with the body guarded. Callable from any thread.
pub fn invoke(f: impl FnOnce() + Send + 'static) -> Result<(), slint::EventLoopError> {
    slint::invoke_from_event_loop(move || guard(f))
}

/// `slint::spawn_local` with every poll of the task guarded: a panic ends the task.
#[allow(dead_code)] // Used on macOS and Windows (the file dialogs) only.
pub fn spawn_local(
    task: impl Future<Output = ()> + 'static,
) -> Result<slint::JoinHandle<()>, slint::EventLoopError> {
    slint::spawn_local(Guarded(Some(Box::pin(task))))
}

/// `Window::on_close_requested` with the handler guarded; a panic keeps the window.
pub fn on_close_requested(
    window: &slint::Window,
    mut f: impl FnMut() -> slint::CloseRequestResponse + 'static,
) {
    window.on_close_requested(move || {
        guard_or(|| slint::CloseRequestResponse::KeepWindowShown, &mut f)
    });
}

/// `Window::set_rendering_notifier` with the notifier guarded.
#[allow(dead_code)] // Used on macOS and Windows only (the preview layer).
pub fn set_rendering_notifier(
    window: &slint::Window,
    mut f: impl FnMut(slint::RenderingState, &slint::GraphicsAPI<'_>) + 'static,
) -> Result<(), slint::SetRenderingNotifierError> {
    window.set_rendering_notifier(move |state, api| guard(|| f(state, api)))
}

/// A task whose polls, and whose drop, run under the panic boundary. After a panic the
/// task is dropped inside the boundary too, so nothing of it unwinds into Slint.
struct Guarded(Option<Pin<Box<dyn Future<Output = ()>>>>);

impl Future for Guarded {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let Some(task) = self.0.as_mut() else {
            return Poll::Ready(());
        };
        let polled = rbs_crash::contain(|| task.as_mut().poll(cx));
        match polled {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(())) => {
                drop_guarded(self.0.take());
                Poll::Ready(())
            }
            Err(_) => {
                crate::show_caught();
                drop_guarded(self.0.take());
                Poll::Ready(())
            }
        }
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        drop_guarded(self.0.take());
    }
}

fn drop_guarded(task: Option<Pin<Box<dyn Future<Output = ()>>>>) {
    if let Some(task) = task {
        guard(move || drop(task));
    }
}
