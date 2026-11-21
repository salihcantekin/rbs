//! The Output profile and Scene collection menus' Import and Export: the system file
//! dialog, then the writer thread's work, both awaited on the UI's event loop.

use super::{CTX, entry, file_dialog};

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
