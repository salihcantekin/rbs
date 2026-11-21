//! Properties' activity rows (ADR-0152): "Runs", "When it comes back", "Keep running
//! for" and a scroll's "When it comes back", each a position in its list.

use super::{App, Ctx, entry};
use crate::ui::Studio;

/// The Studio callbacks of the activity rows, on one window's global.
pub fn install(g: &Studio<'_>, ctx: &Ctx) {
    let c = ctx.clone();
    g.on_set_source_run(move |id, i| {
        entry::guard(|| c.update(move |app: &mut App| app.studio.set_source_run(id, i)));
    });
    let c = ctx.clone();
    g.on_set_source_on_return(move |id, i| {
        entry::guard(|| c.update(move |app: &mut App| app.studio.set_source_on_return(id, i)));
    });
    let c = ctx.clone();
    g.on_set_source_linger(move |id, i| {
        entry::guard(|| c.update(move |app: &mut App| app.studio.set_source_linger(id, i)));
    });
    let c = ctx.clone();
    g.on_set_source_scroll_return(move |id, i| {
        entry::guard(|| {
            c.update(move |app: &mut App| app.studio.set_source_scroll_return(id, i));
        });
    });
}
