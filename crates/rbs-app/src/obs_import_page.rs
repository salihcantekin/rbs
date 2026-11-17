//! The Import from OBS window (ADR-0131 step 3, owner direction 2026-11-16): a two-page
//! wizard opened from Settings › General. The window only shows choices and the
//! report; reading, planning and writing are in `rbs_app::obs_import`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use rbs_app::obs_import::{AppCatalogue, AppLookup, CheckRow, Found};
use rbs_import_obs::{ImportPlan, Mappers, Options, ProfileFiles, SceneChoice};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::ui::{ObsImportWindow, ObsReportRow, ObsSceneChoice, Theme, Tokens};
use crate::{App, Ctx, entry, look, window_place};
use rbs_app::studio_host;

/// How many rows a list on the Check page shows before "N more".
const HEAD: usize = 3;

/// The open wizard and what it has read.
pub struct ObsImport {
    pub window: ObsImportWindow,
    found: Found,
    /// The chosen collection's bytes.
    json: Vec<u8>,
    /// The chosen profile's files, `None` for "No profile".
    profile: Option<ProfileFiles>,
    /// The collection's scenes and whether each is brought.
    scenes: Vec<(String, bool)>,
    /// The plan shown on the Check page.
    plan: Option<ImportPlan>,
}

/// Opens the wizard, or brings it forward.
pub fn open(ctx: &Ctx, app: &mut App) {
    if let Some(imp) = &app.obs_import {
        window_place::raise(imp.window.window());
        return;
    }
    let Ok(w) = ObsImportWindow::new() else {
        app.show_error(crate::UiError::Other(
            "The Import from OBS window cannot be opened.".into(),
        ));
        return;
    };
    rbs_app::window_events::install(&w);
    look::apply(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        ctx.look.borrow().as_ref(),
    );
    let found = rbs_app::obs_import::find_obs();
    w.set_key_place(format!("Kept in {}", app.studio.key_place).into());
    if found.collections.is_empty() {
        w.set_nothing_found(
            "No OBS settings were found on this computer. Choose a scene collection file: in OBS, Scene Collection › Export."
                .into(),
        );
    }
    let c = ctx.clone();
    entry::on_close_requested(w.window(), move || {
        c.update(|app| app.obs_import = None);
        slint::CloseRequestResponse::HideWindow
    });
    install(&w, ctx);
    let t = w.global::<Tokens>();
    let size = slint::LogicalSize::new(
        t.get_size_menu_md() * 2.0 + t.get_space_8() * 2.0,
        t.get_size_floating_h(),
    );
    let mut imp = ObsImport {
        window: w,
        found,
        json: Vec::new(),
        profile: None,
        scenes: Vec::new(),
        plan: None,
    };
    imp.fill_sources(0, 0);
    imp.source_chosen();
    let _ = window_place::show_centred(&imp.window, &app.main, size, app.hidden);
    let _ = crate::exclude_window_from_capture(imp.window.window(), app.bg.exclude_own_window);
    app.capture_applied.1 = usize::MAX;
    app.capture_retry = None;
    app.obs_import = Some(imp);
}

fn install(w: &ObsImportWindow, ctx: &Ctx) {
    macro_rules! on {
        ($name:ident, |$imp:ident, $app:ident $(, $arg:ident : $ty:ty)*| $body:block) => {{
            let ctx = ctx.clone();
            w.$name(move |$($arg: $ty),*| {
                ctx.update(move |$app: &mut App| {
                    if let Some($imp) = $app.obs_import.as_mut() $body
                })
            });
        }};
    }
    on!(on_source_chosen, |imp, app| {
        imp.source_chosen();
    });
    on!(on_scene_toggled, |imp, app, i: i32, on: bool| {
        if let Some(s) = usize::try_from(i).ok().and_then(|i| imp.scenes.get_mut(i)) {
            s.1 = on;
        }
        imp.show_scenes();
    });
    on!(on_all_scenes, |imp, app, on: bool| {
        for s in &mut imp.scenes {
            s.1 = on;
        }
        imp.show_scenes();
    });
    on!(on_check, |imp, app| {
        imp.check();
    });
    {
        let c = ctx.clone();
        w.on_import(move || {
            entry::guard(|| {
                let ctx = c.clone();
                c.update(move |app| {
                    if let Some(mut imp) = app.obs_import.take() {
                        imp.import(&ctx, app);
                        app.obs_import = Some(imp);
                    }
                });
            });
        });
    }
    w.on_choose_file(move || {
        entry::guard(choose_file);
    });
    {
        let c = ctx.clone();
        w.on_dismiss(move || {
            entry::guard(|| {
                c.update(|app| {
                    if let Some(imp) = app.obs_import.take() {
                        let _ = imp.window.hide();
                    }
                });
            });
        });
    }
}

/// The system file dialog for a collection file; runs on the event loop (see
/// `transfer` in main.rs for why it is asynchronous).
fn choose_file() {
    let task = entry::spawn_local(async move {
        let Some(ctx) = crate::CTX.with(|c| c.borrow().clone()) else {
            return;
        };
        let picked = crate::file_dialog::open_json("Choose an OBS scene collection").await;
        ctx.update(move |app| {
            let Some(imp) = app.obs_import.as_mut() else {
                return;
            };
            match picked {
                Ok(None) => {}
                Err(why) => imp.window.set_problem(why.into()),
                Ok(Some(path)) => imp.add_file(path),
            }
        });
    });
    if let Err(e) = task {
        crate::CTX.with(|c| {
            if let Some(ctx) = c.borrow().clone() {
                ctx.update(move |app| {
                    if let Some(imp) = &app.obs_import {
                        imp.window.set_problem(e.to_string().into());
                    }
                });
            }
        });
    }
}

fn rows(rows: &[CheckRow]) -> (ModelRc<ObsReportRow>, ModelRc<ObsReportRow>) {
    let all: Vec<ObsReportRow> = rows
        .iter()
        .map(|r| ObsReportRow {
            name: r.name.as_str().into(),
            place: r.place.as_str().into(),
            reason: r.reason.as_str().into(),
        })
        .collect();
    let head = all.iter().take(HEAD).cloned().collect::<Vec<_>>();
    (
        ModelRc::new(VecModel::from(all)),
        ModelRc::new(VecModel::from(head)),
    )
}

fn strings(names: impl Iterator<Item = String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        names.map(SharedString::from).collect::<Vec<_>>(),
    ))
}

impl ObsImport {
    /// Fills both selects and picks `collection` and `profile` (the profile list ends
    /// with "No profile").
    fn fill_sources(&mut self, collection: usize, profile: usize) {
        let w = &self.window;
        w.set_collections(strings(self.found.collections.iter().map(|c| c.0.clone())));
        w.set_profiles(strings(
            self.found
                .profiles
                .iter()
                .map(|p| p.0.clone())
                .chain(std::iter::once("No profile".to_owned())),
        ));
        let index = |i: usize, len: usize| i32::try_from(i.min(len)).unwrap_or(-1);
        w.set_collection_index(if self.found.collections.is_empty() {
            -1
        } else {
            index(collection, self.found.collections.len() - 1)
        });
        w.set_profile_index(index(profile, self.found.profiles.len()));
    }

    /// Adds a chosen file to the collections and reads it.
    fn add_file(&mut self, path: PathBuf) {
        let name = rbs_import_obs::discover::collection_name(&path);
        self.window.set_file_name(path.display().to_string().into());
        self.found.collections.push((name, path));
        let profile = usize::try_from(self.window.get_profile_index()).unwrap_or(0);
        self.fill_sources(self.found.collections.len() - 1, profile);
        self.window.set_nothing_found(SharedString::new());
        self.source_chosen();
    }

    /// Reads the chosen collection and profile.
    fn source_chosen(&mut self) {
        let w = &self.window;
        w.set_problem(SharedString::new());
        self.plan = None;
        self.json.clear();
        self.scenes.clear();
        self.profile = None;
        let collection = usize::try_from(w.get_collection_index())
            .ok()
            .and_then(|i| self.found.collections.get(i));
        let profile = usize::try_from(w.get_profile_index())
            .ok()
            .and_then(|i| self.found.profiles.get(i));
        let mut problem = None;
        if let Some((_, path)) = collection {
            match rbs_import_obs::discover::read_limited(path) {
                Ok(json) => {
                    self.scenes = rbs_app::obs_import::scene_names(&json)
                        .into_iter()
                        .map(|n| (n, true))
                        .collect();
                    if self.scenes.is_empty() {
                        problem = Some(
                            "The file is not an OBS scene collection, or it has no scenes."
                                .to_owned(),
                        );
                    }
                    self.json = json;
                }
                Err(e) => problem = Some(e.to_string()),
            }
        }
        if let Some((name, dir)) = profile {
            match rbs_import_obs::discover::read_profile(dir) {
                Ok(files) => self.profile = Some(files),
                Err(e) => problem = Some(format!("The profile {name} could not be read: {e}")),
            }
        }
        w.set_has_profile(self.profile.is_some());
        w.set_canvas_summary(
            self.profile
                .as_ref()
                .map(rbs_app::obs_import::canvas_summary)
                .unwrap_or_default()
                .into(),
        );
        w.set_problem(problem.unwrap_or_default().into());
        w.set_picking(false);
        self.show_scenes();
    }

    fn show_scenes(&self) {
        let w = &self.window;
        let on = self.scenes.iter().filter(|s| s.1).count();
        let all = on == self.scenes.len();
        w.set_scenes(ModelRc::new(VecModel::from(
            self.scenes
                .iter()
                .map(|(name, on)| ObsSceneChoice {
                    name: name.as_str().into(),
                    on: *on,
                })
                .collect::<Vec<_>>(),
        )));
        w.set_all_scenes_on(all);
        let summary = if all {
            format!("All {}", self.scenes.len())
        } else {
            format!("{on} of {}", self.scenes.len())
        };
        w.set_scenes_summary(summary.into());
    }

    fn options(&self) -> Options {
        let w = &self.window;
        let all = self.scenes.iter().all(|s| s.1);
        let has = self.profile.is_some();
        Options {
            scenes: if all {
                SceneChoice::All
            } else {
                SceneChoice::Only(
                    self.scenes
                        .iter()
                        .filter(|s| s.1)
                        .map(|s| s.0.clone())
                        .collect::<BTreeSet<_>>(),
                )
            },
            global_audio: w.get_global_audio(),
            audio_filters: w.get_audio_filters(),
            video_filters: w.get_video_filters(),
            hotkeys: w.get_hotkeys(),
            canvas: has && w.get_canvas(),
            recording: has && w.get_recording(),
            stream_server: has && w.get_stream_server(),
            stream_key: has && w.get_stream_server() && w.get_stream_key(),
            ..Options::default()
        }
    }

    /// Builds the plan and shows the Check page.
    fn check(&mut self) {
        let w = &self.window;
        if !self.scenes.iter().any(|s| s.1) {
            w.set_problem("Choose at least one scene.".into());
            return;
        }
        let planned = rbs_import_obs::plan(
            &self.json,
            self.profile.as_ref(),
            &Mappers::builtin(),
            &AppCatalogue,
            &AppLookup,
            &self.options(),
        );
        match planned {
            Err(e) => w.set_problem(e.to_string().into()),
            Ok(plan) => {
                let (exact, approximated, skipped) = rbs_app::obs_import::check_view(&plan.report);
                w.set_exact(exact.into());
                let (all, head) = rows(&approximated);
                w.set_approximated(all);
                w.set_approximated_head(head);
                let (all, head) = rows(&skipped);
                w.set_skipped(all);
                w.set_skipped_head(head);
                self.plan = Some(plan);
                w.set_page(1);
            }
        }
    }

    /// Writes the plan: a new scene collection and output profile, the stream key and
    /// the hotkeys. Nothing the user has now changes.
    fn import(&mut self, ctx: &Ctx, app: &mut App) {
        let Some(plan) = self.plan.clone() else {
            return;
        };
        let w = &self.window;
        w.set_busy(true);
        let result = app.studio.user.import_obs(plan);
        w.set_busy(false);
        match result {
            Err(e) => w.set_problem(format!("Nothing was imported: {e}").into()),
            Ok((written, key)) => {
                if let Some((account, key)) = key {
                    app.studio
                        .send(studio_host::Command::SaveImportedKey(account, key));
                }
                app.studio.refresh_library();
                if written.shortcuts_added > 0 {
                    crate::shortcuts_page::load(ctx, app);
                }
                let mut done = format!("Done. {} is a new scene collection", written.collection.1);
                if let Some((_, name)) = &written.profile {
                    done.push_str(&format!(" and {name} a new output profile"));
                }
                done.push_str(". Switch to them in the Scene collection and Output profile menus.");
                if written.shortcuts_added > 0 {
                    done.push_str(&format!(" {} hotkeys were added.", written.shortcuts_added));
                }
                w.set_done(done.into());
                let notes: Vec<CheckRow> = written.notes.iter().map(CheckRow::from).collect();
                let (all, head) = rows(&notes);
                w.set_approximated(all);
                w.set_approximated_head(head);
                self.plan = None;
                w.set_page(2);
            }
        }
    }
}
