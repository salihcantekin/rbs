//! Starting the studio's control thread (ADR-0041 decision 7): once at launch, and again
//! when the user presses Restart the studio after it stopped
//! ([crash-resilience spec](../../../docs/specs/crash-resilience.md#restart-the-studio)).
//!
//! A restart stops the old studio off the UI thread (ADR-0156: the UI thread never waits
//! for the studio), then starts a new one on the same canvas with the scene collection
//! as last saved. Updates from an older studio are dropped, so nothing it says while it
//! ends reaches the window after the new one started. Nothing here runs while the
//! studio runs (P13): the stop thread and the budget timer exist only during a restart.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_app::stream_key::KeyStore;
use rbs_app::studio_host::{self, HostConfig, StudioHost, Update};
use rbs_core::log::Area;
use rbs_scene::SceneCollection;
use slint::ComponentHandle;

use crate::{App, CTX, Ctx, entry, interact, projector, replay_page, vcam_page};

/// How long the old studio may take to stop: the studio step's budget of the
/// shutdown (ADR-0156 decision 5).
pub(crate) const STOP_BUDGET: Duration = Duration::from_secs(8);

/// Shown after "The studio is not running: " when the old studio did not stop in time.
pub(crate) const STUCK: &str = "the studio did not stop. Quit RBS and open it again.";

/// Which studio may talk to the window: raised by each restart.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Where a restart is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Restart {
    /// No restart runs.
    Idle,
    /// The old studio is stopping; the new one starts when it has.
    Stopping,
    /// The old studio did not stop within [`STOP_BUDGET`].
    Stuck,
}

thread_local! {
    static RESTART: Cell<Restart> = const { Cell::new(Restart::Idle) };
    /// The stream-key store the first studio got, handed to every later one (a
    /// memory store on Linux must keep its keys).
    static KEYS: RefCell<Option<Arc<dyn KeyStore>>> = const { RefCell::new(None) };
}

/// Where a restart is (UI thread).
pub(crate) fn restart_state() -> Restart {
    RESTART.with(Cell::get)
}

/// Whether the window offers Restart the studio: it stopped, and no restart is
/// stopping the old one or waits for it.
pub(crate) fn can_restart(state: i32) -> bool {
    state == 2 && restart_state() == Restart::Idle
}

/// Restart the studio in the picture panels (`Studio.restart-studio`).
pub(crate) fn install(g: &crate::Studio<'_>, ctx: &Ctx) {
    let ctx = ctx.clone();
    g.on_restart_studio(move || {
        entry::guard(|| ctx.update(restart));
    });
}

/// Records where a restart is and hides the action while one runs.
fn set_restart(app: &App, to: Restart) {
    RESTART.with(|r| r.set(to));
    app.main
        .global::<crate::Studio>()
        .set_restart_busy(to != Restart::Idle);
}

/// The first studio, at launch. `keys` is kept for later restarts.
pub(crate) fn first(ctx: &Ctx, keys: Arc<dyn KeyStore>, collection: SceneCollection) {
    KEYS.with(|k| *k.borrow_mut() = Some(Arc::clone(&keys)));
    let config = config(&ctx.app.borrow(), keys, collection);
    let started = spawn(config);
    ctx.update(move |app| match started {
        Ok(host) => {
            app.studio.host = Some(host);
            // Recent actions and connected AI tools redraw the page when they change.
            // Runs on the API thread; `entry::invoke` guards the body on the UI thread.
            app.studio.ai_forward.notify_with(|| {
                let _ = entry::invoke(|| {
                    if let Some(ctx) = CTX.with(|c| c.borrow().clone()) {
                        ctx.update(|_| {});
                    }
                });
            });
            running(app);
        }
        Err(e) => {
            app.studio.state = 2;
            app.studio.failure = e.to_string();
        }
    });
}

/// What follows a studio that runs, at launch and after a restart.
fn running(app: &mut App) {
    // Remote control, if the user turned it on (off by default: nothing starts).
    app.studio.sync_remote();
    // AI tools, if "Start when RBS opens" is on (ADR-0134 decision 2).
    app.studio.ai_autostart();
    vcam_page::studio_started(app);
    replay_page::started(app);
}

/// Restart the studio: only while it is stopped and no restart runs.
pub(crate) fn restart(app: &mut App) {
    if !can_restart(app.studio.state) {
        return;
    }
    rbs_core::info!(Area::Studio, "restarting the studio");
    // Nothing the old studio says from now on reaches the window.
    GENERATION.fetch_add(1, Ordering::Relaxed);
    // The layers' frames go back before the old studio and its publishers stop.
    #[cfg(any(target_os = "macos", windows))]
    crate::preview::close();
    projector::close_all();
    interact::close_all();
    app.preview_endpoint = None;
    app.scene_preview_endpoint = None;
    app.vertical_preview_endpoint = None;
    let old = app.studio.host.take();
    // Without a studio, remote control and AI tools stop; `running` starts them again.
    app.studio.sync_remote();
    app.studio.state = 0;
    app.studio.failure.clear();
    set_restart(app, Restart::Stopping);
    let stopper = std::thread::Builder::new()
        .name("rbs-studio-stop".into())
        .spawn(move || {
            // Joins the old control thread, which closes what it still holds.
            drop(old);
            let _ = entry::invoke(stopped);
        });
    if let Err(e) = stopper {
        // No thread: the old studio was dropped with the closure; start at once.
        rbs_core::warn!(Area::Studio, "the stop thread did not start ({e})");
        set_restart(app, Restart::Idle);
        start_again(app);
        return;
    }
    entry::single_shot(STOP_BUDGET, past_budget);
}

/// The stop budget ended (UI thread).
fn past_budget() {
    with_app(|app| {
        if restart_state() != Restart::Stopping {
            return;
        }
        rbs_core::error!(
            Area::Studio,
            "the old studio did not stop in {STOP_BUDGET:?}"
        );
        set_restart(app, Restart::Stuck);
        app.studio.state = 2;
        app.studio.failure = STUCK.to_owned();
    });
}

/// The old studio stopped (UI thread): start the new one, or, past the budget, offer
/// the restart again.
fn stopped() {
    with_app(|app| match restart_state() {
        Restart::Idle => {}
        Restart::Stopping => {
            set_restart(app, Restart::Idle);
            start_again(app);
        }
        Restart::Stuck => {
            rbs_core::info!(Area::Studio, "the old studio stopped late");
            set_restart(app, Restart::Idle);
            app.studio.failure = "the studio stopped.".to_owned();
        }
    });
}

/// A new studio on the same canvas, with the collection in use as last saved.
fn start_again(app: &mut App) {
    let id = app.studio.user.collection_id.clone();
    let collection = match app.studio.user.load_collection(&id) {
        Ok((_, collection)) => collection,
        Err(e) => {
            app.studio.state = 2;
            app.studio.failure = format!("the scenes could not be read: {e}");
            return;
        }
    };
    let Some(keys) = KEYS.with(|k| k.borrow().clone()) else {
        return;
    };
    match spawn(config(app, keys, collection)) {
        Ok(host) => {
            app.studio.host = Some(host);
            running(app);
        }
        Err(e) => {
            app.studio.state = 2;
            app.studio.failure = e.to_string();
        }
    }
}

fn with_app(f: impl FnOnce(&mut App) + 'static) {
    if let Some(ctx) = CTX.with(|c| c.borrow().clone()) {
        ctx.update(f);
    }
}

/// The studio's configuration from the window's settings and this computer's files.
fn config(app: &App, keys: Arc<dyn KeyStore>, collection: SceneCollection) -> HostConfig {
    let c = app.studio.canvas;
    let mut config = HostConfig::canvas(c.size(), c.frame_rate());
    config.keys = keys;
    config.collection = Some(collection);
    config.autosave = app.studio.user.collection_saver();
    config.key_account = rbs_app::stream_key::account(app.studio.user.profile_id.as_str());
    config.virtual_camera = app.vcam.factory();
    rbs_app::platform::host_seams(&mut config);
    config.config_dir = Some(app.config.clone());
    config.swap_after_transition = app.bg.swap_after_transition;
    config.media_pause_release = app.bg.pause_release();
    // The monitor device belongs to this computer (ADR-0097); a file that cannot be
    // read gives System default at full volume and is left as it is.
    let (monitor, note) = rbs_app::monitor_settings::load(&app.config);
    if let Some(why) = note {
        rbs_core::warn!(Area::App, "monitor settings: {why}");
    }
    config.monitor = monitor;
    // Show loudness, the target and the true-peak limit (ADR-0092; `loudness.toml`).
    let (loudness, note) = rbs_app::loudness_settings::load(&app.config);
    if let Some(why) = note {
        rbs_core::warn!(Area::App, "loudness settings: {why}");
    }
    config.loudness = loudness;
    // This computer's calibration (ADR-0087 decision 4), read with `app.toml`
    // before the control thread serves anything.
    let (calibration, note) = rbs_app::calibration_file::load(&app.config);
    if let Some(why) = note {
        rbs_core::warn!(Area::App, "calibration: {why}");
    }
    config.calibration = calibration;
    config.read_free_space = Some(studio_host::FreeSpaceReader(Arc::new(
        |folder: &std::path::Path| {
            rbs_system_probe::volume_space(folder)
                .map(|s| s.free_bytes)
                .map_err(|e| e.to_string())
        },
    )));
    // The built-in add-on catalog (ADR-0145); nothing is read until asked.
    config.add_ons = rbs_app::add_ons::system_setup();
    config
}

/// Starts a control thread whose updates reach the window while it is the newest.
fn spawn(config: HostConfig) -> std::io::Result<StudioHost> {
    let generation = GENERATION.load(Ordering::Relaxed);
    StudioHost::spawn(
        config,
        rbs_app::platform::adapters,
        || {
            // A hardware adapter, else a software one (a virtual machine, for example):
            // slow, but the app still works.
            let ctx = match rbs_gpu::GpuContext::new(&rbs_gpu::GpuRequest::default()) {
                Err(rbs_gpu::GpuError::NoAdapter) => {
                    rbs_gpu::GpuContext::new(&rbs_gpu::GpuRequest {
                        allow_software: true,
                        ..rbs_gpu::GpuRequest::default()
                    })
                }
                other => other,
            };
            ctx.map(Arc::new).map_err(|e| format!("no usable GPU: {e}"))
        },
        || {
            let mut engine = rbs_engine::Engine::new();
            engine.set_thread_policy(Some(Arc::new(rbs_thread_os::OsThreadPolicy::new())));
            engine
        },
        move |u: Update| {
            let _ = entry::invoke(move || {
                if GENERATION.load(Ordering::Relaxed) != generation {
                    return;
                }
                if let Some(ctx) = CTX.with(|c| c.borrow().clone()) {
                    ctx.studio_update(u);
                }
            });
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(r: Restart) {
        RESTART.with(|c| c.set(r));
    }

    #[test]
    fn offered_only_while_stopped_and_no_restart_runs() {
        at(Restart::Idle);
        assert!(can_restart(2));
        assert!(!can_restart(0), "starting");
        assert!(!can_restart(1), "running");
        at(Restart::Stopping);
        assert!(!can_restart(2));
        at(Restart::Stuck);
        assert!(!can_restart(2));
        at(Restart::Idle);
    }
}
