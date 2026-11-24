//! Quit (ADR-0056 decision 3, ADR-0156): the question while a stream or a recording
//! runs, taking every window away inside the event loop, and the studio's shutdown on
//! its own thread, each step within a budget.

use std::cell::{Cell, RefCell};

use rbs_app::background::{self, Action, LiveState, Request};
use rbs_app::shutdown;
use slint::ComponentHandle;

#[cfg(any(target_os = "macos", windows))]
use crate::preview;
use crate::studio_ui::{self, VerticalOut, rec_state, st_state};
use crate::ui::*;
use crate::{App, Ctx, entry, interact, projector};

thread_local! {
    /// Quit is decided and the shutdown runs (ADR-0156 decision 3): no window comes
    /// back, no request is decided again and studio updates are dropped.
    static QUITTING: Cell<bool> = const { Cell::new(false) };
    /// Follows the shutdown thread's steps (ADR-0156 decision 5).
    static SHUTDOWN: RefCell<Option<shutdown::Watcher>> = const { RefCell::new(None) };
    /// The shutdown's work when its thread could not start: run after the loop.
    static SHUTDOWN_INLINE: RefCell<Option<Vec<shutdown::Work>>> = const { RefCell::new(None) };
}

/// Whether Quit is decided ([`QUITTING`]).
pub(crate) fn quitting() -> bool {
    QUITTING.with(Cell::get)
}

/// Looks at the shutdown's progress: ends the loop when every step has finished, ends
/// the process when a step ran past its budget, and otherwise looks again when the
/// running step's budget is over (ADR-0156 decisions 5 and 6).
fn check_shutdown() {
    let state = SHUTDOWN.with(|s| s.borrow_mut().as_mut().map(shutdown::Watcher::check));
    match state {
        None => {}
        Some(shutdown::State::Finished) => {
            SHUTDOWN.with(|s| s.borrow_mut().take());
            entry::quit_loop();
        }
        Some(shutdown::State::Running { left, .. }) => entry::single_shot(left, check_shutdown),
        Some(shutdown::State::OverBudget(step)) => {
            rbs_core::error!(
                rbs_core::log::Area::App,
                "shutdown did not finish within its budget: {}",
                step.name()
            );
            rbs_crash::exit_unfinished(step.name());
        }
    }
}

/// Quits after a measuring run (`measure_ui`): Quit without the question, on the
/// next turn, since nothing goes out while measuring.
pub(crate) fn quit_after_measure() {
    entry::single_shot(std::time::Duration::ZERO, || {
        let ctx = crate::CTX.with(|c| c.borrow().clone());
        match ctx {
            Some(ctx) => match ctx.app.try_borrow_mut() {
                Ok(mut app) => {
                    let started = std::time::Instant::now();
                    ctx.begin_quit(&mut app);
                    crate::measure_ui::report_close(started.elapsed());
                }
                Err(_) => entry::quit_loop(),
            },
            None => entry::quit_loop(),
        }
    });
}

/// What a quit would end now (ADR-0156 decision 1): a stream from Go live until it has
/// ended, a recording from its start until it has stopped, main or vertical.
fn live_state(s: &studio_ui::StudioUi) -> LiveState {
    LiveState {
        streaming: s.st_state != st_state::OFF || s.vertical_st != VerticalOut::Off,
        recording: s.rec_state != rec_state::OFF || s.vertical_rec != VerticalOut::Off,
        problem: false,
    }
}

impl Ctx {
    /// A Close or a Quit (ADR-0056 decision 3, ADR-0156 decision 1). Returns what the
    /// close handler answers the toolkit.
    pub(crate) fn request(&self, request: Request) -> slint::CloseRequestResponse {
        self.decide(request, true);
        slint::CloseRequestResponse::KeepWindowShown
    }

    /// Decides `request`. When the state is borrowed by a callback in progress, it is
    /// decided once more on the next turn (`retry`), then dropped: never a quit without
    /// asking.
    fn decide(&self, request: Request, retry: bool) {
        if quitting() {
            return;
        }
        let Ok(mut app) = self.app.try_borrow_mut() else {
            if retry {
                let ctx = self.clone();
                entry::single_shot(std::time::Duration::ZERO, move || {
                    ctx.decide(request, false);
                });
            } else {
                rbs_core::warn!(
                    rbs_core::log::Area::App,
                    "a close or quit request was dropped: the app was busy"
                );
            }
            return;
        };
        let live = live_state(&app.studio);
        match background::on_request(&app.bg, request, live, Self::way_back()) {
            Action::Hide { notice: true } => {
                // Said once, in the frame, before the windows go (OK hides them).
                let kind = if cfg!(target_os = "macos") { 1 } else { 2 };
                app.main.global::<Shell>().set_background_notice(kind);
            }
            Action::Hide { notice: false } => {
                self.hide_all(&mut app);
            }
            Action::AskBeforeQuit(ending) => {
                // Ending a stream or a recording cannot be undone: the frame asks first
                // (U4), in a window the user can see.
                if app.hidden {
                    self.show_all(&mut app);
                }
                app.main.window().set_minimized(false);
                let shell = app.main.global::<Shell>();
                shell.set_quit_ending(ending.code());
                shell.set_quit_confirm(true);
            }
            Action::Quit => self.begin_quit(&mut app),
        }
    }

    /// Quit (ADR-0156 decisions 3 to 5): saves the layout, takes every window, layer
    /// and the tray away inside the event loop, then stops the studio on the shutdown
    /// thread. The loop runs on until the last step has finished.
    pub(crate) fn begin_quit(&self, app: &mut App) {
        QUITTING.with(|q| q.set(true));
        self.queue.borrow_mut().clear();
        app.exiting = true;
        if app.save_due {
            app.persist_now();
        }
        self.save_timer.stop();
        self.meter_timer.stop();
        self.live_timer.stop();
        #[cfg(target_os = "macos")]
        rbs_app::native::begin_quit_activity();
        // A second launch now waits for this RBS to end (ADR-0181 decision 2).
        crate::single::closing();
        app.main.global::<Shell>().set_quit_confirm(false);
        if let Some(w) = app.settings.take() {
            let _ = w.hide();
            app.studio.settings_closed();
        }
        for w in [
            app.source_settings.as_ref().map(|w| w.window()),
            app.sync_measure.as_ref().map(|w| w.window()),
            app.obs_import.as_ref().map(|i| i.window.window()),
            app.attributes.as_ref().map(|w| w.window()),
        ]
        .into_iter()
        .flatten()
        {
            let _ = w.hide();
        }
        for f in &app.floats {
            let _ = f.window.hide();
        }
        let _ = app.main.hide();
        // The layers' frames go back before the studio and its publisher stop.
        #[cfg(any(target_os = "macos", windows))]
        preview::close();
        projector::close_all();
        interact::close_all();
        // No tray icon while closing (ADR-0156 decision 7); the single-instance event
        // went with `single::closing` above.
        #[cfg(windows)]
        {
            app.tray = None;
        }
        // What stops off this thread, in `shutdown::STEPS` order.
        let remote = app.studio.stop_remote();
        let host = app.studio.host.take();
        let saver = app.studio.user.take_saver();
        let work: Vec<shutdown::Work> = vec![
            Box::new(move || drop(remote)),
            Box::new(move || drop(host)),
            Box::new(move || {
                if let Some(saver) = saver {
                    saver.flush();
                    drop(saver);
                }
            }),
        ];
        let clock = std::sync::Arc::new(rbs_core::time::MonotonicClock::new());
        match shutdown::start(work, clock, || {
            let _ = entry::invoke(check_shutdown);
        }) {
            Ok(watcher) => {
                SHUTDOWN.with(|s| *s.borrow_mut() = Some(watcher));
                check_shutdown();
            }
            Err(not_started) => {
                rbs_core::error!(
                    rbs_core::log::Area::App,
                    "the shutdown thread did not start ({}); stopping after the loop",
                    not_started.error
                );
                SHUTDOWN_INLINE.with(|s| *s.borrow_mut() = Some(not_started.work));
                entry::quit_loop();
            }
        }
    }

    /// After the event loop: Quit did all of it inside the loop (ADR-0156 decisions 3
    /// to 5), unless the shutdown thread could not start, whose work runs here; a loop
    /// that ended some other way stops as before ADR-0156.
    pub(crate) fn after_loop(&self, main: &MainWindow) {
        let ctx = self;
        if let Some(work) = SHUTDOWN_INLINE.with(|s| s.borrow_mut().take()) {
            for step in work {
                let _ = rbs_crash::contain(step);
            }
        } else if !quitting() {
            // The loop ended some other way: stop as before ADR-0156.
            let _ = main.hide();
            // The layer's frames go back before the studio and its publisher stop.
            #[cfg(any(target_os = "macos", windows))]
            preview::close();
            projector::close_all();
            interact::close_all();
            if let Ok(mut app) = ctx.app.try_borrow_mut() {
                if app.save_due {
                    app.persist_now();
                }
                // Disconnects remote control apps, then stops the studio and joins its thread.
                app.studio.stop_remote();
                app.studio.host = None;
                // The last changes reach the disk before the app ends.
                app.studio.user.flush();
            }
        }
    }
}
