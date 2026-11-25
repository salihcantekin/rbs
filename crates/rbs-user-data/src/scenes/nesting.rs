//! Refuse legacy nested-scene placements before upgrading the document.

use crate::document::BodyError;

pub(super) fn check_before_upgrade(body: &toml::Table) -> Result<(), BodyError> {
    let nested = body
        .get("scenes")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table)
        .filter_map(|scene| scene.get("items"))
        .filter_map(toml::Value::as_array)
        .flatten()
        .filter_map(toml::Value::as_table)
        .any(|item| item.contains_key("scene"));
    if nested {
        Err(BodyError::Scene(rbs_scene::SceneError::NestingUnsupported))
    } else {
        Ok(())
    }
}
