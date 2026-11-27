use rbs_api::request::{METHODS, UI_METHODS};
use rbs_core::log::Area;
use rbs_ui::command::{CommandId, Entry, Kind};

use super::PanelId;

fn missing_method(entry: &Entry) -> Option<&'static str> {
    let missing = |name: &&str| {
        !METHODS.iter().any(|method| method.name == *name) && !UI_METHODS.contains(name)
    };
    match entry.kind {
        Kind::Action { method } | Kind::Picker { method, .. } => [method].into_iter().find(missing),
        Kind::Toggle { on, off } | Kind::ResolvedToggle { on, off } => {
            [on, off].into_iter().find(missing)
        }
    }
}

/// The UI registry retains its own behavior; both halves of a toggle must still
/// refer to the shared catalog or its client-scoped UI method list.
pub(super) fn check_entry(
    entry: Option<&Entry>,
    report: impl FnOnce(&'static str, &'static str),
) -> Option<&Entry> {
    let entry = entry?;
    if let Some(method) = missing_method(entry) {
        report(entry.id, method);
        return None;
    }
    Some(entry)
}

pub fn resolve_catalogued_entry(id: &CommandId) -> Option<&'static Entry> {
    check_entry(id.resolve().ok()?, |entry, method| {
        rbs_core::error!(
            Area::Ui,
            "command registry {entry} refers to uncatalogued method {method}; update the registry or control API catalog"
        );
    })
}

pub fn panel_of(id: &CommandId) -> Option<PanelId> {
    PanelId::parse(id.param("id")?).ok()
}
