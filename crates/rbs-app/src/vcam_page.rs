//! Settings › Virtual camera in the shell (ADR-0066 decisions 11 and 12): the
//! `VirtualCamera` global's callbacks, the work on the camera off the UI thread, and
//! the section's state ([`rbs_app::vcam_settings::Page`]).
//!
//! Adding and removing the camera may wait for an administrator prompt, so each runs on
//! a thread of its own that ends with the work (nothing runs while the section is not
//! used, P13); the result comes back to the UI thread through `CTX`. Sending runs on
//! the studio's control thread ([`studio_host::Command::StartVirtualCamera`]).

use std::path::Path;

use rbs_app::platform::VirtualCamera as Platform;
use rbs_app::studio_host::{self, CameraStatus, Command};
use rbs_app::vcam_settings::{self, Done, Page, RATES, SIZES, Sending, VcamSettings, Work};
use rbs_core::media::PixelFormat;
use rbs_vcam::{CameraFormat, InstallState, VcamError};
use slint::{Model, ModelRc, SharedString, VecModel};

use super::{App, CTX, Ctx, entry};
use crate::ui::VirtualCamera;

/// The section's state and the platform's camera.
pub struct Camera {
    page: Page,
    platform: Option<Platform>,
}

impl Camera {
    /// The camera of this platform, with the settings in `config`. Also returns a note
    /// when the settings file could not be read.
    pub fn load(config: &Path) -> (Self, Option<String>) {
        let (settings, note) = vcam_settings::load(config);
        let (platform, no_adapter) = match rbs_app::platform::virtual_camera(config) {
            Ok(p) => (Some(p), None),
            Err(why) => (None, Some(why)),
        };
        let page = Page::new(settings, no_adapter);
        (Self { page, platform }, note)
    }

    /// The camera for the studio's control thread.
    pub fn factory(&self) -> Option<studio_host::CameraFactory> {
        self.platform
            .as_ref()
            .map(|p| studio_host::CameraFactory(std::sync::Arc::clone(&p.factory)))
    }

    /// The studio started or stopped sending.
    pub fn sending(&mut self, status: Option<CameraStatus>) {
        self.page.sending(status.map(|s| Sending {
            width: s.width,
            height: s.height,
            rate: s.rate,
            counters: s.counters,
        }));
    }
}

/// Sets the section's callbacks on a Settings window's global.
pub fn install(g: &VirtualCamera<'_>, ctx: &Ctx) {
    let c = ctx.clone();
    g.on_toggle(move |on| {
        c.update(move |app| {
            app.vcam.page.note = None;
            if on {
                app.vcam.page.confirming = true;
            } else {
                app.vcam.page.confirming = false;
                set_enabled(app, false);
                run(app, Work::Removing);
            }
        });
    });
    let c = ctx.clone();
    g.on_add(move || {
        c.update(move |app| {
            app.vcam.page.confirming = false;
            app.vcam.page.note = None;
            run(app, Work::Adding);
        });
    });
    let c = ctx.clone();
    g.on_cancel(move || c.update(|app| app.vcam.page.confirming = false));
    let c = ctx.clone();
    g.on_set_size(move |i| {
        c.update(move |app| {
            if let Some((w, h)) = usize::try_from(i).ok().and_then(|i| SIZES.get(i)) {
                let s = &mut app.vcam.page.settings;
                (s.width, s.height) = (*w, *h);
                format_changed(app);
            }
        });
    });
    let c = ctx.clone();
    g.on_set_rate(move |i| {
        c.update(move |app| {
            if let Some(r) = usize::try_from(i).ok().and_then(|i| RATES.get(i)) {
                app.vcam.page.settings.rate = *r;
                format_changed(app);
            }
        });
    });
    let c = ctx.clone();
    g.on_send(move |on| {
        c.update(move |app| {
            let page = &mut app.vcam.page;
            page.note = None;
            if on {
                let s = page.settings;
                page.starting = true;
                app.studio.send(Command::StartVirtualCamera {
                    width: s.width,
                    height: s.height,
                    fps: s.frame_rate(),
                });
            } else {
                app.studio.send(Command::StopVirtualCamera);
            }
        });
    });
    let c = ctx.clone();
    g.on_remove_all(move || {
        c.update(move |app| {
            app.vcam.page.note = None;
            app.vcam.page.confirming = false;
            set_enabled(app, false);
            run(app, Work::RemovingAll);
        });
    });
}

/// The section was shown: read the camera's state again (it may have changed outside
/// RBS, for example with `rbs-cli vcam`).
pub fn opened(app: &mut App) {
    if app.vcam.page.work.is_none() {
        run(app, Work::Checking);
    }
}

/// Shows the section's state in the Settings window.
pub fn push(app: &App) {
    let Some(w) = &app.settings else {
        return;
    };
    let g = slint::ComponentHandle::global::<VirtualCamera>(w);
    let page = &app.vcam.page;
    let v = page.view();
    g.set_available(v.available);
    g.set_added(v.added);
    g.set_can_toggle(v.can_toggle);
    g.set_state(v.state.into());
    g.set_tone(v.tone);
    g.set_confirming(page.confirming);
    if g.get_sizes().row_count() == 0 {
        let sizes: Vec<SharedString> = SIZES
            .iter()
            .map(|(w, h)| SharedString::from(format!("{w} x {h}")))
            .collect();
        g.set_sizes(ModelRc::new(VecModel::from(sizes)));
        let rates: Vec<SharedString> = RATES
            .iter()
            .map(|r| SharedString::from(format!("{r} fps")))
            .collect();
        g.set_rates(ModelRc::new(VecModel::from(rates)));
    }
    g.set_size(page.settings.size_index() as i32);
    g.set_rate(page.settings.rate_index() as i32);
    g.set_can_format(v.can_format);
    g.set_format_help(v.format_help.into());
    g.set_sending(page.sending.is_some());
    g.set_can_send(v.can_send);
    g.set_sending_text(v.sending_text.into());
    g.set_can_remove_all(v.can_remove_all);
}

fn set_enabled(app: &mut App, on: bool) {
    let s = &mut app.vcam.page.settings;
    if s.enabled != on {
        s.enabled = on;
        save(app);
    }
}

fn save(app: &mut App) {
    if let Err(e) = vcam_settings::save(&app.config, &app.vcam.page.settings) {
        app.vcam.page.note = Some(format!("The setting was not saved: {e}"));
    }
}

/// Size or Frame rate changed: saved, and announced when the camera is added.
fn format_changed(app: &mut App) {
    app.vcam.page.note = None;
    save(app);
    if app.vcam.page.state == Some(InstallState::Installed) {
        run(app, Work::Formatting);
    }
}

/// The format `s` names, NV12.
fn camera_format(s: &VcamSettings) -> CameraFormat {
    CameraFormat::new(s.width, s.height, s.frame_rate(), PixelFormat::Nv12)
}

fn words(e: &VcamError) -> String {
    match e {
        VcamError::Unsupported(why) | VcamError::PermissionDenied(why) => format!("{why}."),
        VcamError::NotInstalled => "The camera is not added.".into(),
        e => format!("{e}."),
    }
}

/// Runs `work` on a thread of its own and hands the result to the UI thread.
fn run(app: &mut App, work: Work) {
    let Some(platform) = app.vcam.platform.clone() else {
        return;
    };
    if app.vcam.page.work.is_some() {
        return;
    }
    // Stopping first: the camera is not removed while RBS sends to it.
    if matches!(work, Work::Removing | Work::RemovingAll) && app.vcam.page.sending.is_some() {
        app.studio.send(Command::StopVirtualCamera);
    }
    app.vcam.page.work = Some(work);
    let format = camera_format(&app.vcam.page.settings);
    let started = std::thread::Builder::new()
        .name("rbs-vcam-setup".into())
        .spawn(move || {
            rbs_crash::report_panics_on_this_thread();
            let state = |r: Result<InstallState, VcamError>| match r {
                Ok(s) => Done::State(s),
                Err(e) => Done::Failed(words(&e)),
            };
            let f = &*platform.factory;
            let mut done = Vec::new();
            match work {
                Work::Checking => done.push(state(f.status())),
                Work::Adding => {
                    let added = state(f.install());
                    let ok = added == Done::State(InstallState::Installed);
                    done.push(added);
                    if ok {
                        done.push(match f.set_format(&format) {
                            Ok(applied) => Done::Format(applied),
                            Err(e) => Done::Failed(words(&e)),
                        });
                    }
                }
                Work::Removing => done.push(state(f.uninstall())),
                Work::RemovingAll => done.push(state((platform.remove_completely)())),
                Work::Formatting => done.push(match f.set_format(&format) {
                    Ok(applied) => Done::Format(applied),
                    Err(e) => Done::Failed(words(&e)),
                }),
            }
            // After a failure, the state as it is now.
            if done.iter().any(|d| matches!(d, Done::Failed(_)))
                && work != Work::Checking
                && let Ok(s) = f.status()
            {
                done.push(Done::State(s));
            }
            let _ = entry::invoke(move || {
                let ctx = CTX.with(|c| c.borrow().clone());
                if let Some(ctx) = ctx {
                    ctx.update(move |app| finished(app, work, done));
                }
            });
        });
    if started.is_err() {
        app.vcam.page.work = None;
        app.vcam.page.note = Some("RBS could not start the work on the camera.".into());
    }
}

fn finished(app: &mut App, work: Work, done: Vec<Done>) {
    let page = &mut app.vcam.page;
    // The last state wins; a failure note stays.
    let mut failed = None;
    for d in done {
        match d {
            Done::Failed(why) => failed = Some(why),
            d => page.done(work, d),
        }
    }
    page.work = None;
    if let Some(why) = failed {
        page.done(work, Done::Failed(why));
    }
    if work == Work::Adding && page.state == Some(InstallState::Installed) {
        set_enabled(app, true);
    }
}
