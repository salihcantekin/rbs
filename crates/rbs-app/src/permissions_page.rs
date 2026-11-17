//! Settings › Permissions (ADR-0141 decisions 7, 9 and 10): the rows the host's
//! statuses give, the row actions and the "who gets these permissions" card.
//!
//! Statuses are read by the studio's control thread when the page is shown, when the
//! Settings window becomes active again and on Check again
//! ([`Command::ReadPermissions`]); a prompt runs there on a one-shot worker
//! ([`Command::RequestPermission`]). Nothing runs while the page is not used (P13).

use rbs_app::permission_text::{self, Action, Os, Tone};
use rbs_app::studio_host::Command;
use rbs_sources::permissions::{PermissionKind, PermissionStatus};
use slint::{ComponentHandle, ModelRc, VecModel};

use super::{App, Ctx, entry};
use crate::ui::{Overlays, PermissionRowData, Studio};

/// The page's section number in the Settings window.
pub const SECTION: i32 = 11;

/// The Studio callbacks of the page, on one window's global.
pub fn install(g: &Studio<'_>, ctx: &Ctx) {
    let c = ctx.clone();
    g.on_permission_check(move || entry::guard(|| c.update(opened)));
    let c = ctx.clone();
    g.on_permission_action(move |kind, action| {
        entry::guard(|| {
            c.update(move |app| {
                let Some(kind) = permission_text::kind_of_code(kind) else {
                    return;
                };
                match Action::from_code(action) {
                    Action::Allow => app.studio.send(Command::RequestPermission(kind)),
                    Action::OpenSettings => {
                        crate::privacy::open(kind);
                    }
                    Action::None => {}
                }
            });
        });
    });
}

/// The page was shown, its window became active, or Check again: read the statuses.
pub fn opened(app: &mut App) {
    app.studio.send(Command::ReadPermissions);
}

/// The first-run page's access step: Later or Done closes the page.
pub fn install_first_run(g: &Overlays<'_>, ctx: &Ctx) {
    let c = ctx.clone();
    g.on_first_run_access_done(move || {
        entry::guard(|| {
            c.update(|app| {
                let o = app.main.global::<Overlays>();
                o.set_first_run_open(false);
                o.set_first_run_step(0);
            });
        });
    });
}

/// The kinds the first-run page offers (ADR-0141 decision 11).
const FIRST_RUN: [PermissionKind; 3] = [
    PermissionKind::Camera,
    PermissionKind::Microphone,
    PermissionKind::Screen,
];

/// Shows the statuses in the Settings window when the page is open, and on the
/// first-run page's access step.
pub fn push(app: &mut App) {
    push_first_run(app);
    let Some(w) = &app.settings else {
        return;
    };
    // Only while the page shows: nothing is built for a closed page (P13).
    if w.get_section() != SECTION {
        return;
    }
    let g = w.global::<Studio>();
    set_rows(&g, rows(app, |_| true));
    let os = Os::current();
    let id = rbs_system_probe::app_identity::cached();
    let h = permission_text::holder(id, os);
    if g.get_permission_holder() != h.value.as_str() {
        g.set_permission_holder(h.value.into());
        g.set_permission_holder_path(h.path.into());
        g.set_permission_holder_note(h.note.into());
        g.set_permission_holder_warn(h.warn);
    }
}

/// The first-run page waits for the statuses after the experience level: it shows
/// the access step when a kind is not asked yet or blocked, else it closes. While the
/// step shows, its rows follow the statuses.
fn push_first_run(app: &mut App) {
    let o = app.main.global::<Overlays>();
    if let Some(asked) = app.first_run_access {
        if app.studio.permissions_serial == asked {
            return;
        }
        app.first_run_access = None;
        let (rows, _) = &app.studio.permissions;
        let open = rows.iter().any(|(k, i)| {
            FIRST_RUN.contains(k)
                && matches!(
                    i.status,
                    PermissionStatus::NotAsked | PermissionStatus::Blocked
                )
        });
        if !open {
            o.set_first_run_open(false);
            return;
        }
        o.set_first_run_step(1);
    }
    if !o.get_first_run_open() || o.get_first_run_step() != 1 {
        return;
    }
    let data = rows(app, |k| FIRST_RUN.contains(&k));
    o.set_first_run_access_to_do(data.iter().any(|r| r.action != 0 || r.tone != 0));
    set_rows(&app.main.global::<Studio>(), data);
}

fn set_rows(g: &Studio<'_>, data: Vec<PermissionRowData>) {
    let current: Vec<PermissionRowData> = {
        use slint::Model;
        g.get_permission_rows().iter().collect()
    };
    if current != data {
        g.set_permission_rows(ModelRc::new(VecModel::from(data)));
    }
}

/// The rows of the kinds `keep` keeps, in page order.
fn rows(app: &App, keep: impl Fn(PermissionKind) -> bool) -> Vec<PermissionRowData> {
    let os = Os::current();
    let id = rbs_system_probe::app_identity::cached();
    let starter = id
        .foreign_app()
        .as_deref()
        .and_then(rbs_system_probe::app_identity::app_name);
    let (rows, waiting) = &app.studio.permissions;
    rows.iter()
        .filter(|(kind, _)| keep(*kind))
        .map(|(kind, info)| {
            let r = permission_text::row(
                *kind,
                *info,
                *waiting == Some(*kind),
                crate::privacy::pane(*kind).is_some(),
                os,
                starter.as_deref(),
            );
            PermissionRowData {
                kind: permission_text::kind_code(*kind),
                label: r.label.into(),
                status: r.status.into(),
                tone: match r.tone {
                    Tone::Plain => 0,
                    Tone::Warning => 1,
                    Tone::Problem => 2,
                },
                value: r.value.into(),
                action: r.action.code(),
                action_text: match r.action {
                    Action::Allow => "Allow".into(),
                    Action::OpenSettings => "Open system settings".into(),
                    Action::None => "".into(),
                },
            }
        })
        .collect()
}
