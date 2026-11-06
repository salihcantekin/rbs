//! U20 (ADR-0050, design system spec "Commands, menus and the toolbar"): every command
//! registry entry runs control API methods only: methods `api.describe` lists, or the
//! client-scoped `ui.*` methods of the control API spec.

use rbs_api::request::METHODS;
use rbs_ui::command::ENTRIES;

/// The client-scoped `ui.*` methods of the control API spec ("Client-scoped UI
/// commands"). They are routed to the UI client, so `api.describe` does not list them.
const UI_METHODS: &[&str] = &[
    "ui.workspace.list",
    "ui.workspace.switch",
    "ui.panel.open",
    "ui.panel.close",
    "ui.area.show",
    "ui.area.hide",
    "ui.area.toggle",
    "ui.panel.collapse",
    "ui.panel.expand",
    "ui.panel.auto_hide",
    "ui.panel.pin",
    "ui.area.hide_all",
    "ui.area.show_all",
    "ui.panel.maximize",
    "ui.panel.restore",
    "ui.layout.studio_split",
    "ui.layout.reset_sizes",
    "ui.toolbar.get",
    "ui.toolbar.set",
    "ui.toolbar.reset",
    "ui.command.list",
    "ui.command.run",
];

#[test]
fn every_entry_runs_control_api_methods() {
    for e in ENTRIES {
        for m in e.kind.methods() {
            let known = METHODS.iter().any(|i| i.name == m) || UI_METHODS.contains(&m);
            assert!(known, "{} runs {m}, which is not a control API method", e.id);
        }
    }
}
