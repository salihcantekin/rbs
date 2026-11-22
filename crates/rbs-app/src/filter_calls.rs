//! The Filters tab's callbacks: choose, order and edit a source's filters, and the
//! source's own Show the cursor that Pointer halo shows (ADR-0165, owner 2026-11-22).

use super::{App, Ctx};
use crate::ui::Studio;

/// The filter callbacks, on one window's global.
pub fn install(g: &Studio<'_>, ctx: &Ctx) {
    macro_rules! on {
        ($name:ident, |$app:ident $(, $arg:ident : $ty:ty)*| $body:block) => {{
            let ctx = ctx.clone();
            g.$name(move |$($arg: $ty),*| ctx.update(move |$app: &mut App| $body));
        }};
    }
    on!(on_select_filter, |app, source: i32, filter: i32| {
        app.studio.select_filter(source, filter);
    });
    on!(on_remove_filter, |app, filter: i32| {
        app.studio.remove_filter(filter);
    });
    on!(on_move_filter, |app, filter: i32, index: i32| {
        app.studio.move_filter(filter, index);
    });
    on!(on_set_filter_enabled, |app, filter: i32, on: bool| {
        app.studio.set_filter_enabled(filter, on);
    });
    on!(
        on_preview_filter,
        |app, filter: i32, field: i32, position: f32| {
            app.studio.preview_filter(filter, field, position);
        }
    );
    on!(
        on_set_filter_source,
        |app, filter: i32, field: i32, option: i32| {
            app.studio.set_filter_source(filter, field, option);
        }
    );
    on!(
        on_set_filter_option,
        |app, filter: i32, field: i32, option: i32| {
            app.studio.set_filter_option(filter, field, option);
        }
    );
    on!(on_reset_filter_field, |app, filter: i32, field: i32| {
        app.studio.reset_filter_field(filter, field);
    });
    on!(on_commit_filter, |app, filter: i32| {
        app.studio.commit_filter(filter);
    });
    on!(on_cancel_filter, |app, filter: i32| {
        app.studio.cancel_filter(filter);
    });
    on!(
        on_set_filter_colour,
        |app, filter: i32, field: i32, colour: slint::SharedString| {
            app.studio.set_filter_colour(filter, field, &colour);
        }
    );
    on!(on_reset_filter, |app, filter: i32| {
        app.studio.reset_filter(filter);
    });
    on!(on_set_source_cursor, |app, id: i32, on: bool| {
        app.studio.set_source_cursor(id, on)
    });
}
