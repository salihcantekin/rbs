//! Tests of the Properties folds (Collapse all) and the fold state helpers.

use crate::{fold_ids, fold_state};

#[test]
fn fold_state_reads_open_some_and_all_closed() {
    let closed = |id: &str| id != "a";
    assert_eq!(fold_state("", closed), 0);
    assert_eq!(fold_state(" , ", closed), 0);
    assert_eq!(fold_state("a,b", closed), 1);
    assert_eq!(fold_state("b,c", closed), 2);
    assert_eq!(fold_ids("a, b,,c").collect::<Vec<_>>(), ["a", "b", "c"]);
}

/// Collapse all in Properties folds the ids `SourceProperties` lists, so the list
/// must name every section Properties draws.
#[test]
fn properties_folds_name_every_properties_section() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/studio.slint");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    // SourceProperties' computed list and the fixed list GroupProperties sets.
    let folds: String = ["property <string> folds:", "Studio.properties-folds = \""]
        .iter()
        .map(|m| {
            text.find(m)
                .map_or("", |s| &text[s..s + text[s..].find(';').unwrap_or(0)])
        })
        .collect();
    let mut ids = 0;
    for line in text.lines() {
        if let Some(id) = line.trim().strip_prefix("id: \"properties.") {
            let id = format!("properties.{}", id.trim_end_matches("\";"));
            ids += 1;
            assert!(
                folds.contains(&format!("{id},")) || folds.contains(&format!("{id}\"")),
                "{id} is missing from SourceProperties' folds"
            );
        }
    }
    assert!(ids > 10, "found {ids} Properties sections");
}
