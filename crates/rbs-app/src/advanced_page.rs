//! Settings › Advanced (ADR-0158 decision 8, spec `diagnostics-log.md`): Log detail,
//! its per-area levels, and the logs for a bug report.
//!
//! A level is saved in `app.toml` and applied at once (`rbs_app::diag_log::apply`).
//! Save logs writes its zip on a thread of its own, never on the UI thread; nothing
//! runs while the page is not used (P13).

use std::path::PathBuf;

use rbs_app::background;
use rbs_app::diag_log::{self, AREA_NAMES, LogDetail};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{App, CTX, Ctx, UiError, entry};
use crate::ui::{LogAreaRow, Studio};

/// The page's section number in the Settings window.
pub const SECTION: i32 = 13;

/// The issue form Report on GitHub opens (`.github/ISSUE_TEMPLATE/bug_report.yml`).
const NEW_ISSUE: &str = "/issues/new?template=bug_report.yml";

/// The Studio callbacks of the page, on one window's global.
pub fn install(g: &Studio<'_>, ctx: &Ctx) {
    macro_rules! on {
        ($name:ident, |$app:ident $(, $arg:ident : $ty:ty)*| $body:block) => {{
            let ctx = ctx.clone();
            g.$name(move |$($arg: $ty),*| ctx.update(move |$app: &mut App| $body));
        }};
    }
    on!(on_set_log_detail, |app, i: i32| {
        let detail = LogDetail::from_index(i);
        app.bg.log_detail = (detail != LogDetail::Normal).then_some(detail);
        changed(app);
    });
    on!(on_set_log_back_to_normal, |app, on: bool| {
        app.bg.log_keep = !on;
        changed(app);
    });
    on!(on_set_log_area, |app, name: SharedString, level: i32| {
        if level < 0 {
            app.bg.log_areas.remove(name.as_str());
        } else {
            app.bg
                .log_areas
                .insert(name.to_string(), LogDetail::from_index(level));
        }
        changed(app);
    });
    on!(on_reset_log_areas, |app| {
        app.bg.log_areas.clear();
        changed(app);
    });
    on!(on_open_logs_folder, |app| {
        match diag_log::folder() {
            Some(dir) => {
                let _ = std::fs::create_dir_all(&dir);
                crate::reveal::show_in_folder(&dir);
            }
            None => app.show_error(UiError::Other("This system has no logs folder.".to_owned())),
        }
    });
    on!(on_report_problem, |app| {
        let url = format!(
            "{}{NEW_ISSUE}&version={}&os={}",
            env!("CARGO_PKG_REPOSITORY"),
            env!("CARGO_PKG_VERSION"),
            format_args!("{}+{}", std::env::consts::OS, std::env::consts::ARCH),
        );
        if !crate::reveal::open_url(&url) {
            app.show_error(UiError::Other(format!("RBS could not open {url}")));
        }
    });
    on!(on_save_logs, |app| {
        save(app);
    });
}

/// Saves `app.toml`, applies the levels and shows them.
fn changed(app: &mut App) {
    if let Err(e) = background::save(&app.config, &app.bg) {
        app.show_error(UiError::Other(format!("The setting was not saved: {e}")));
    }
    diag_log::apply(&app.bg);
    push(app);
}

/// Shows the page's state in the Settings window while the page is open.
pub fn push(app: &App) {
    let Some(w) = &app.settings else {
        return;
    };
    if w.get_section() != SECTION {
        return;
    }
    let g = w.global::<Studio>();
    let detail = app.bg.log_detail.unwrap_or_default();
    g.set_log_detail(detail.index());
    g.set_log_detailed(detail.is_detailed() || app.bg.log_areas.values().any(|d| d.is_detailed()));
    g.set_log_back_to_normal(!app.bg.log_keep);
    g.set_log_env(diag_log::env_levels().unwrap_or("").into());
    g.set_log_areas_changed(!app.bg.log_areas.is_empty());
    g.set_can_save_logs(diag_log::folder().is_some());
    let rows: Vec<LogAreaRow> = AREA_NAMES
        .iter()
        .map(|(area, label)| LogAreaRow {
            name: area.name().into(),
            label: (*label).into(),
            level: app.bg.log_areas.get(area.name()).map_or(-1, |d| d.index()),
        })
        .collect();
    let current: Vec<LogAreaRow> = {
        use slint::Model;
        g.get_log_areas().iter().collect()
    };
    if current != rows {
        g.set_log_areas(ModelRc::new(VecModel::from(rows)));
    }
}

/// Shows what the last Save logs did.
fn status(app: &App, text: String, tone: i32) {
    if let Some(w) = &app.settings {
        let g = w.global::<Studio>();
        g.set_log_save_status(text.into());
        g.set_log_save_tone(tone);
    }
}

/// Asks where to save (the system dialog; on Linux the logs folder) and writes the zip
/// on its own thread.
fn save(app: &mut App) {
    let name = format!(
        "rbs-logs-{}.zip",
        rbs_log::stamp(std::time::SystemTime::now())
    );
    let config = app.config.clone();
    let settings = toml::to_string(&app.bg).unwrap_or_default();
    status(app, "Saving the logs…".to_owned(), 0);
    let task = entry::spawn_local(async move {
        let picked = where_to_save(&name).await;
        let Some(out) = (match picked {
            Ok(p) => p,
            Err(why) => {
                report(Err(why));
                return;
            }
        }) else {
            report(Ok(None));
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("rbs-save-logs".to_owned())
            .spawn(move || {
                let done = diag_log::save_logs(&out, &config, settings)
                    .map(|n| Some((out, n)))
                    .map_err(|e| format!("The logs were not saved: {e}"));
                let _ = entry::invoke(move || report(done));
            });
        if let Err(e) = spawned {
            report(Err(format!("The logs were not saved: {e}")));
        }
    });
    if let Err(e) = task {
        status(app, format!("The logs were not saved: {e}"), 2);
    }
}

/// On the UI thread: the outcome of a save.
fn report(done: Result<Option<(PathBuf, usize)>, String>) {
    let Some(ctx) = CTX.with(|c| c.borrow().clone()) else {
        return;
    };
    ctx.update(move |app| match done {
        Ok(None) => status(app, String::new(), 0),
        Ok(Some((path, n))) => {
            status(app, format!("Saved {n} files in {}.", path.display()), 1);
            crate::reveal::show_in_folder(&path);
        }
        Err(why) => status(app, why, 2),
    });
}

#[cfg(any(target_os = "macos", windows))]
async fn where_to_save(name: &str) -> Result<Option<PathBuf>, String> {
    Ok(rfd::AsyncFileDialog::new()
        .set_title("Save logs")
        .add_filter("Zip archive", &["zip"])
        .set_file_name(name)
        .save_file()
        .await
        .map(|f| f.path().to_owned()))
}

/// No file dialog on this system yet: the zip goes into the logs folder, which Save
/// logs then shows.
#[cfg(not(any(target_os = "macos", windows)))]
async fn where_to_save(name: &str) -> Result<Option<PathBuf>, String> {
    let dir = diag_log::folder().ok_or("This system has no logs folder.")?;
    Ok(Some(dir.join(name)))
}
