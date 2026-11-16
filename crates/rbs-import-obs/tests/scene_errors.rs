//! The spec's degrade table (docs/specs/obs-import.md, "Degrading on scene validation")
//! names every `SceneError` variant. `SceneError` is `#[non_exhaustive]`, so a match in
//! this crate cannot be exhaustive; instead this test reads the enum's source and fails
//! when a variant is added that the table does not name.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

const SCENE_SOURCE: &str = include_str!("../../rbs-scene/src/collection.rs");
const SPEC: &str = include_str!("../../../docs/specs/obs-import.md");

fn variants() -> Vec<&'static str> {
    let start = SCENE_SOURCE
        .find("pub enum SceneError {")
        .expect("SceneError is in collection.rs");
    let body = &SCENE_SOURCE[start..];
    let body = &body[body.find('{').unwrap() + 1..body.find("\n}").unwrap()];
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//") && !l.starts_with("#["))
        .map(|l| {
            let end = l
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(l.len());
            &l[..end]
        })
        .filter(|v| !v.is_empty())
        .collect()
}

#[test]
fn every_scene_error_has_a_degrade_action() {
    let table_start = SPEC
        .find("### Degrading on scene validation")
        .expect("the spec has the degrade table");
    let table = &SPEC[table_start..];
    let table = &table[..table.find("\n\n## ").unwrap_or(table.len())];
    let found = variants();
    assert!(found.len() >= 30, "parsed {found:?}");
    for v in found {
        assert!(
            table.contains(&format!("`{v}`")),
            "SceneError::{v} has no row in the obs-import spec's degrade table"
        );
    }
}
