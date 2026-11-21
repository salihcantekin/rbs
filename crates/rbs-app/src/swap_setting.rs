//! Settings › General › Swap after transition: kept in the background settings file,
//! which the window owns (ADR-0163 Mirror row, ADR-0164 decision 5).

use rbs_app::studio_host::Command;
use slint::ComponentHandle;

use super::{App, Studio, UiError, background};

/// The switch: the studio uses it and the file keeps it.
pub fn set(app: &mut App, on: bool) {
    if app.bg.swap_after_transition != on {
        app.studio.send(Command::SetSwapAfterTransition(on));
        changed(app, on);
    }
}

/// `settings.set` changed it, or the switch did: shown and saved.
pub fn changed(app: &mut App, on: bool) {
    app.bg.swap_after_transition = on;
    if let Err(e) = background::save(&app.config, &app.bg) {
        app.show_error(UiError::Other(format!("The setting was not saved: {e}")));
    }
    app.main.global::<Studio>().set_swap_after_transition(on);
    if let Some(w) = &app.settings {
        w.global::<Studio>().set_swap_after_transition(on);
    }
}
