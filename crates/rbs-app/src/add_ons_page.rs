//! Settings › Add-ons (ADR-0145 decision 9): one row per catalog entry and its
//! command.
//!
//! The studio's control thread reads the statuses when the page is shown
//! ([`Command::ListAddOns`]) and reports changes as they happen; nothing runs while
//! the page is not used (P13).

use rbs_app::add_on_text::{self, Action};
use rbs_app::studio_host::{AddOnAction, Command};
use slint::{ComponentHandle, ModelRc, VecModel};

use super::{App, Ctx, entry};
use crate::ui::{AddOnRowData, Studio};

/// The page's section number in the Settings window.
pub const SECTION: i32 = 12;

/// The Studio callbacks of the page, on one window's global.
pub fn install(g: &Studio<'_>, ctx: &Ctx) {
    let c = ctx.clone();
    g.on_add_on_action(move |id, action| {
        entry::guard(|| {
            c.update(move |app| {
                let action = match Action::from_code(action) {
                    Action::Download | Action::Retry => AddOnAction::Install,
                    Action::Update => AddOnAction::Update,
                    Action::Cancel => AddOnAction::Cancel,
                    Action::Remove => AddOnAction::Remove,
                    Action::None => return,
                };
                app.studio.send(Command::AddOn {
                    id: id.to_string(),
                    action,
                });
            });
        });
    });
}

/// The page was shown: read the statuses.
pub fn opened(app: &mut App) {
    app.studio.send(Command::ListAddOns);
}

/// Shows the rows in the Settings window while the page is open.
pub fn push(app: &App) {
    let Some(w) = &app.settings else {
        return;
    };
    // Only while the page shows: nothing is built for a closed page (P13).
    if w.get_section() != SECTION {
        return;
    }
    let data: Vec<AddOnRowData> = app
        .studio
        .add_ons
        .iter()
        .map(|info| {
            let r = add_on_text::row(info);
            AddOnRowData {
                id: r.id.into(),
                label: r.label.into(),
                help: r.help.into(),
                problem: r.problem,
                value: r.value.into(),
                action: r.action.code(),
                action_text: r.action.text().into(),
            }
        })
        .collect();
    let g = w.global::<Studio>();
    let current: Vec<AddOnRowData> = {
        use slint::Model;
        g.get_add_on_rows().iter().collect()
    };
    if current != data {
        g.set_add_on_rows(ModelRc::new(VecModel::from(data)));
    }
}
