//! U20 (ADR-0050, design system spec "Commands, menus and the toolbar"): every command
//! registry entry runs control API methods only: methods `api.describe` lists, or the
//! client-scoped `ui.*` methods of the control API spec.

use rbs_api::request::{METHODS, UI_METHODS};
use rbs_ui::command::ENTRIES;

#[test]
fn every_entry_runs_control_api_methods() {
    for e in ENTRIES {
        for m in e.kind.methods() {
            let known = METHODS.iter().any(|i| i.name == m) || UI_METHODS.contains(&m);
            assert!(
                known,
                "{} runs {m}, which is not a control API method",
                e.id
            );
        }
    }
}
