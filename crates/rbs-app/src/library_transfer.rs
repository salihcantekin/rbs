//! The Output profile and Scene collection menus' Import and Export: the system file
//! dialog, then the writer thread's work, both awaited on the UI's event loop.

use std::cell::Cell;

use slint::ComponentHandle;

use super::{App, CTX, Overlays, close_preview, entry, file_dialog, menu_bar};

/// Which library a menu imports into or exports from.
#[derive(Clone, Copy)]
pub(crate) enum Library {
    Profiles,
    Collections,
}

/// Import (`import`) a file into a library, or export the entry in use, named `name`,
/// to a file. The system file dialog runs asynchronously on the UI's event loop: a
/// modal dialog would run a nested event loop inside a window event, which winit does
/// not allow on macOS (the app aborted on Export).
pub(crate) fn transfer(library: Library, import: bool, name: String) {
    let task = entry::spawn_local(async move {
        let Some(ctx) = CTX.with(|c| c.borrow().clone()) else {
            return;
        };
        let what = match library {
            Library::Profiles => "output profile",
            Library::Collections => "scene collection",
        };
        let picked = if import {
            file_dialog::open(&format!("Import {what}")).await
        } else {
            file_dialog::save(&format!("Export {name}"), &format!("{name}.toml")).await
        };
        let then = ctx.clone();
        ctx.update(move |app| {
            let path = match picked {
                Ok(Some(path)) => path,
                done => return app.library_done(done.map(drop)),
            };
            // The writer thread does the work; the list and the note follow when it ran.
            let done = move |r: Result<(), String>| {
                then.update(move |app| {
                    app.studio.refresh_library();
                    app.library_done(r);
                })
            };
            let user = &app.studio.user;
            match (library, import) {
                (Library::Profiles, true) => {
                    entry::when_done(user.import_profile_later(&path), move |r| done(r.map(drop)))
                }
                (Library::Profiles, false) => {
                    entry::when_done(user.export_profile_later(&path), done)
                }
                (Library::Collections, true) => {
                    entry::when_done(user.import_collection_later(&path), move |r| {
                        done(r.map(drop))
                    })
                }
                (Library::Collections, false) => {
                    entry::when_done(user.export_collection_later(&path), done)
                }
            }
        });
    });
    if let Err(e) = task {
        CTX.with(|c| {
            if let Some(ctx) = c.borrow().clone() {
                ctx.update(move |app| app.library_done(Err(e.to_string())));
            }
        });
    }
}

thread_local! {
    /// The name prompt's change is on the writer thread (the UI thread's state).
    pub(crate) static NAME_WRITING: Cell<bool> = const { Cell::new(false) };
}

/// Awaits a profile or collection change on the writer thread, finishes it on the UI
/// thread and hands the result to `then`.
pub(crate) fn when_written(
    write: crate::studio_ui::EntryWrite,
    then: impl FnOnce(&mut App, Result<(), String>) + 'static,
) {
    let written = async move { Ok::<_, String>(write.written().await) };
    entry::when_done(written, move |finish| {
        let Some(ctx) = CTX.with(|c| c.borrow().clone()) else {
            return;
        };
        ctx.update(move |app| {
            let done = finish.and_then(|f| f(&mut app.studio, crate::close_preview));
            then(app, done);
        });
    });
}

/// Writer-thread work a studio answer started (the collection a switch left is
/// deleted): its error shows once it ran.
pub(crate) fn when_left(write: Option<crate::studio_ui::EntryWrite>) {
    if let Some(write) = write {
        when_written(write, App::library_done);
    }
}

impl App {
    /// Shows why an Output profile or Scene collection menu action did not work.
    /// The name prompt's change ran: it closes, or a refusal stays in it as its error
    /// line.
    pub(crate) fn name_answered(&mut self, done: Result<(), String>) {
        NAME_WRITING.set(false);
        let o = self.main.global::<Overlays>();
        match done {
            Ok(()) => o.set_name_open(false),
            Err(e) => o.set_name_error(e.into()),
        }
    }

    /// Runs `op` on the Output profile or Scene collection in use; `name` is the new
    /// entry's name (trimmed) for New, Duplicate and Rename; `Some` while the writer
    /// thread still does its part.
    pub(crate) fn entry_command(
        &mut self,
        kind: menu_bar::EntryKind,
        op: menu_bar::EntryOp,
        name: &str,
    ) -> Result<Option<crate::studio_ui::EntryWrite>, String> {
        use menu_bar::{EntryKind, EntryOp};
        let name = name.trim();
        if matches!(op, EntryOp::New | EntryOp::Duplicate | EntryOp::Rename) && name.is_empty() {
            return Err("Give it a name.".to_owned());
        }
        let copy = op == EntryOp::Duplicate;
        match (kind, op) {
            (EntryKind::Profile, EntryOp::New | EntryOp::Duplicate) => {
                self.studio.new_profile(name, copy).map(Some)
            }
            (EntryKind::Profile, EntryOp::Rename) => Ok(Some(self.studio.rename_profile(name))),
            (EntryKind::Profile, EntryOp::Delete) => {
                let id = self.studio.user.profile_id.as_str().to_owned();
                self.studio.delete_profile(&id).map(Some)
            }
            (EntryKind::Collection, EntryOp::New | EntryOp::Duplicate) => {
                self.dismiss_source_settings();
                self.studio.new_collection(name, copy).map(Some)
            }
            (EntryKind::Collection, EntryOp::Rename) => {
                Ok(Some(self.studio.rename_collection(name)))
            }
            (EntryKind::Collection, EntryOp::Delete) => {
                self.dismiss_source_settings();
                let id = self.studio.user.collection_id.as_str().to_owned();
                self.studio.delete_collection(&id, close_preview)
            }
            (EntryKind::Profile, EntryOp::Import | EntryOp::Export) => {
                let name = self.studio.user.profile_name.clone();
                transfer(Library::Profiles, op == EntryOp::Import, name);
                Ok(None)
            }
            (EntryKind::Collection, EntryOp::Import | EntryOp::Export) => {
                let name = self.studio.user.collection_name.clone();
                transfer(Library::Collections, op == EntryOp::Import, name);
                Ok(None)
            }
        }
    }
}
