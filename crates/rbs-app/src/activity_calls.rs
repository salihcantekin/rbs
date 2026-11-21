//! Properties' activity rows (ADR-0152): "Runs", "When it comes back", "Keep running
//! for" and a scroll's "When it comes back", each a position in its list.

use super::{App, Ctx, entry};
use crate::ui::Studio;

/// The Studio callbacks of the activity rows, on one window's global.
pub fn install(g: &Studio<'_>, ctx: &Ctx) {
    let on = |pick: fn(&mut App, i32, i32)| {
        let c = ctx.clone();
        move |id: i32, index: i32| entry::guard(|| c.update(move |app| pick(app, id, index)))
    };
    g.on_set_source_run(on(|app, id, i| app.studio.set_source_run(id, i)));
    g.on_set_source_on_return(on(|app, id, i| app.studio.set_source_on_return(id, i)));
    g.on_set_source_linger(on(|app, id, i| app.studio.set_source_linger(id, i)));
    g.on_set_source_scroll_return(on(|app, id, i| app.studio.set_source_scroll_return(id, i)));
}
