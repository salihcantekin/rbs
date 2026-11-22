//! Source calls that pick a file or folder (moved from `main.rs`): a media file
//! (ADR-0108) and a slide show's folder with its Source tab rows (ADR-0171).

use super::{App, CTX, Ctx, UiError, entry, file_dialog};
use crate::ui::Studio;

/// The Studio callbacks of these rows, on one window's global.
pub fn install(g: &Studio<'_>, ctx: &Ctx) {
    g.on_change_media_file(|id| entry::guard(|| change_media_file(id)));
    g.on_choose_slides_folder(|id| entry::guard(|| choose_folder(id)));
    let c = ctx.clone();
    g.on_slides_step(move |id, direction| {
        entry::guard(|| c.update(move |app: &mut App| app.studio.slides_step(id, direction)));
    });
    let c = ctx.clone();
    g.on_set_slides_option(move |id, field, option| {
        entry::guard(|| {
            c.update(move |app: &mut App| app.studio.set_slides_option(id, field, option));
        });
    });
    let c = ctx.clone();
    g.on_preview_slides_number(move |id, field, p| {
        entry::guard(|| {
            c.update(move |app: &mut App| app.studio.preview_slides_number(id, field, p));
        });
    });
    let c = ctx.clone();
    g.on_commit_slides_number(move |id, field| {
        entry::guard(|| {
            c.update(move |app: &mut App| app.studio.commit_slides_number(id, field));
        });
    });
    let c = ctx.clone();
    g.on_cancel_slides_number(move |id| {
        entry::guard(|| c.update(move |app: &mut App| app.studio.cancel_slides_number(id)));
    });
    let c = ctx.clone();
    g.on_reset_slides_field(move |id, field| {
        entry::guard(|| c.update(move |app: &mut App| app.studio.reset_slides_field(id, field)));
    });
}

/// Asks for a folder of pictures, then runs `done` with it on the UI thread.
fn ask_folder(done: impl FnOnce(&mut App, std::path::PathBuf) + 'static) {
    let task = entry::spawn_local(async move {
        let Some(ctx) = CTX.with(|c| c.borrow().clone()) else {
            return;
        };
        let picked = file_dialog::open_folder("Choose a folder of pictures").await;
        ctx.update(move |app| match picked {
            Ok(None) => {}
            Err(why) => app.show_error(UiError::Other(why)),
            Ok(Some(folder)) => done(app, folder),
        });
    });
    if let Err(e) = task {
        CTX.with(|c| {
            if let Some(ctx) = c.borrow().clone() {
                ctx.update(move |app| app.show_error(UiError::Other(e.to_string())));
            }
        });
    }
}

/// Add source → Slide show.
pub fn pick_folder_to_add() {
    ask_folder(|app, folder| app.studio.add_slideshow_folder(&folder));
}

/// Choose… on the Source tab: another folder, one undo step.
fn choose_folder(id: i32) {
    ask_folder(move |app, folder| {
        app.studio.set_media_setting(
            id,
            rbs_sources::slideshow::FOLDER,
            serde_json::Value::String(folder.to_string_lossy().into_owned()),
        );
    });
}

/// Add source → Media file. The dialog is asynchronous (ADR-0055 / ADR-0108).
pub fn pick_media() {
    let task = entry::spawn_local(async move {
        let Some(ctx) = CTX.with(|c| c.borrow().clone()) else {
            return;
        };
        let picked = file_dialog::open_media("Choose a media file").await;
        ctx.update(move |app| match picked {
            Ok(None) => {}
            Err(why) => app.show_error(UiError::Other(why)),
            Ok(Some(path)) => app.studio.add_media_file(&path),
        });
    });
    if let Err(e) = task {
        CTX.with(|c| {
            if let Some(ctx) = c.borrow().clone() {
                ctx.update(move |app| app.show_error(UiError::Other(e.to_string())));
            }
        });
    }
}

/// Properties › Source › Change… of a media file: the same dialog as Add source, and
/// the new path is one undo step.
fn change_media_file(id: i32) {
    let task = entry::spawn_local(async move {
        let Some(ctx) = CTX.with(|c| c.borrow().clone()) else {
            return;
        };
        let picked = file_dialog::open_media("Choose a media file").await;
        ctx.update(move |app| match picked {
            Ok(None) => {}
            Err(why) => app.show_error(UiError::Other(why)),
            Ok(Some(path)) => app.studio.set_media_setting(
                id,
                "path",
                serde_json::Value::String(path.to_string_lossy().into_owned()),
            ),
        });
    });
    if let Err(e) = task {
        CTX.with(|c| {
            if let Some(ctx) = c.borrow().clone() {
                ctx.update(move |app| app.show_error(UiError::Other(e.to_string())));
            }
        });
    }
}
