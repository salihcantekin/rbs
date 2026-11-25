//! The Sources list's callbacks: select, remove and delete a source, the "Capture
//! audio too" companion and its row under the picture (ADR-0096, ADR-0173), global
//! sources (ADR-0065), and the Properties of a selected whole group (ADR-0184).

use super::{App, Ctx};
use crate::ui::Studio;

/// The Sources list callbacks, on one window's global.
pub fn install(g: &Studio<'_>, ctx: &Ctx) {
    macro_rules! on {
        ($name:ident, |$app:ident $(, $arg:ident : $ty:ty)*| $body:block) => {{
            let ctx = ctx.clone();
            g.$name(move |$($arg: $ty),*| ctx.update(move |$app: &mut App| $body));
        }};
    }
    on!(on_set_companion_audio, |app, id: i32, on: bool| {
        app.studio.set_companion_audio(id, on);
    });
    on!(on_open_sound, |app, id: i32, open: bool| {
        app.studio.open_sound(id, open);
    });
    on!(on_open_group, |app, group: i32, open: bool| {
        app.studio.open_group(group, open);
    });
    on!(on_select_group, |app, group: i32| {
        app.studio.select_group(group);
    });
    on!(
        on_rename_group,
        |app, group: i32, name: slint::SharedString| {
            app.studio.rename_group(group, &name);
        }
    );
    on!(on_set_group_visible, |app, group: i32, on: bool| {
        app.studio.set_group_visible(group, on);
    });
    on!(on_set_group_locked, |app, group: i32, on: bool| {
        app.studio.set_group_locked(group, on);
    });
    on!(on_remove_group, |app, group: i32| {
        app.studio.remove_group(group);
    });
    on!(on_group_set_show_for, |app, ms: i32| {
        app.studio.group_set_show_for(ms);
    });
    on!(on_group_show, |app| {
        app.studio.group_show();
    });
    on!(on_group_set_box, |app,
                           x: f32,
                           y: f32,
                           w: f32,
                           h: f32,
                           gesture: i32| {
        app.studio.group_set_box(x, y, w, h, gesture);
    });
    on!(
        on_group_set_scroll,
        |app,
         on: bool,
         dir: i32,
         speed: f32,
         gap: f32,
         repeat: i32,
         start: i32,
         fade: f32,
         gesture: i32| {
            app.studio
                .group_set_scroll(on, dir, speed, gap, repeat, start, fade, gesture);
        }
    );
    on!(on_group_scroll_live, |app, action: i32, speed: f32| {
        app.studio.group_scroll_live(action, speed);
    });
    on!(on_delete_source, |app, id: i32| {
        app.studio.delete_source(id);
    });
    on!(on_remove_from_scene, |app, id: i32| {
        app.studio.remove_from_scene(id);
    });
    on!(on_set_global, |app, id: i32, global: bool| {
        app.studio.set_global(id, global);
    });
    on!(on_remove_global, |app, id: i32| {
        app.studio.remove_global(id);
    });
    on!(on_pick_global_kind, |app, index: i32| {
        app.studio.pick_global_kind(index);
    });
    on!(on_add_global, |app, option: i32| {
        app.studio.add_global(option);
    });
    on!(on_select_source, |app, id: i32| {
        app.studio.select_one(id);
        app.studio.reveal_sound();
        app.studio.refresh_marks();
    });
    on!(
        on_select_source_with,
        |app, id: i32, ctrl: bool, shift: bool| {
            app.studio.select_with(id, ctrl, shift);
        }
    );
}
