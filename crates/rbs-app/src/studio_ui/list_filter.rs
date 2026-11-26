//! Which rows of the Sources list the panel filter hides (ADR-0195 decision 5). The
//! filter changes only the list: a hidden row stays in the scene and keeps its place.

use std::collections::{HashMap, HashSet};

use rbs_user_data::PanelFilters;

use super::SourceRow;

/// The filter category of a row: its type's, or "other" for a type this build does not
/// know.
fn category(row: &SourceRow) -> &'static str {
    row.kind.map_or("other", |k| {
        rbs_sources::source_category(k.type_id().as_str())
    })
}

/// For each row, whether the filter takes it out of the list. A group is listed while
/// any member is, a sound under its picture follows the picture, and a selected row is
/// always listed.
pub(super) fn filtered_out(
    rows: &[SourceRow],
    filters: &PanelFilters,
    selected: &[i32],
) -> Vec<bool> {
    let hides = |r: &SourceRow| {
        filters
            .hidden_source_categories
            .iter()
            .any(|c| c == category(r))
            || (!r.visible && !filters.show_hidden_sources)
    };
    let mut group_listed: HashSet<_> = HashSet::new();
    for r in rows {
        if let Some(g) = r.group
            && !hides(r)
        {
            group_listed.insert(g);
        }
    }
    let own: HashMap<i32, bool> = rows
        .iter()
        .map(|r| {
            let out = match r.group {
                Some(g) => !group_listed.contains(&g),
                None => hides(r),
            };
            (r.id.get() as i32, out)
        })
        .collect();
    rows.iter()
        .map(|r| {
            let id = r.id.get() as i32;
            if selected.contains(&id) {
                return false;
            }
            match r.companion_of {
                Some(p) => own.get(&(p.get() as i32)).copied().unwrap_or(false),
                None => own[&id],
            }
        })
        .collect()
}
