//! Whether the window reads levels, told to the control thread (F-15 of the 2026-11
//! resource audit; ADR-0169 decision 6; spec `docs/specs/audio-graph.md`, "Mixer").
//!
//! The window reads them while a Mixer is shown in a window that is not hidden, and
//! while a noise gate's bar in Properties reads its levels (its 30 Hz timer, which
//! exists only while the gate's section is open; a second without a read ends it).
//! With every window hidden (Keep RBS running, ADR-0056) nothing reads them, so the
//! mixer drops them and the audio branch rests unless something else reads it.

use std::cell::Cell;
use std::time::Duration;

use crate::studio_ui::StudioUi;
use crate::{App, Ctx};
use rbs_app::studio_host::Command;

/// A gate bar that has not read its levels for this long is gone.
const GATE_QUIET: Duration = Duration::from_secs(1);

/// Who in the window reads levels, and what the control thread was last told.
#[derive(Default)]
pub(crate) struct Readers {
    sent: Cell<bool>,
    mixer: Cell<bool>,
    gate: Cell<bool>,
    quiet: slint::Timer,
}

impl Readers {
    /// The Mixer is shown in a window that is not hidden, or not.
    pub(crate) fn mixer(&self, studio: &StudioUi, shown: bool) {
        self.mixer.set(shown);
        self.send(studio);
    }

    /// Every window is hidden: nothing reads.
    pub(crate) fn hidden(&self, studio: &StudioUi) {
        self.mixer.set(false);
        self.gate.set(false);
        self.quiet.stop();
        self.send(studio);
    }

    fn send(&self, studio: &StudioUi) {
        let on = self.mixer.get() || self.gate.get();
        if on != self.sent.get() {
            studio.send(Command::SetMetersUi(on));
            self.sent.set(on);
        }
    }
}

/// A gate bar's read of `filter`'s levels: the window reads levels until a read is
/// [`GATE_QUIET`] late.
pub(crate) fn gate_levels(ctx: &Ctx, app: &App, filter: i32) -> (f32, f32) {
    let r = &app.meters_ui;
    if app.hidden {
        return app.studio.filter_levels(filter);
    }
    if r.gate.get() {
        r.quiet.restart();
    } else {
        r.gate.set(true);
        r.send(&app.studio);
        let ctx = ctx.clone();
        r.quiet
            .start(slint::TimerMode::SingleShot, GATE_QUIET, move || {
                if let Ok(app) = ctx.app.try_borrow() {
                    app.meters_ui.gate.set(false);
                    app.meters_ui.send(&app.studio);
                }
            });
    }
    app.studio.filter_levels(filter)
}
