//! ADR-0044 permits schema generation in the control API only as an opt-in feature.

use std::collections::BTreeSet;

use toml::Value;

pub fn check(manifest: &Value) -> Result<(), &'static str> {
    let optional = manifest
        .get("dependencies")
        .and_then(|deps| deps.get("schemars"))
        .and_then(|dep| dep.get("optional"))
        .and_then(Value::as_bool);
    if optional != Some(true) {
        return Err("rbs-api's schemars dependency must be optional");
    }
    let features = manifest.get("features").and_then(Value::as_table);
    fn enables_schema(
        name: &str,
        features: Option<&toml::map::Map<String, Value>>,
        visited: &mut BTreeSet<String>,
    ) -> bool {
        if name == "dep:schemars" || name == "schemars" || name.starts_with("schemars/") {
            return true;
        }
        if !visited.insert(name.to_owned()) {
            return false;
        }
        features
            .and_then(|table| table.get(name))
            .and_then(Value::as_array)
            .is_some_and(|members| {
                members
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|member| enables_schema(member, features, visited))
            })
    }
    if enables_schema("default", features, &mut BTreeSet::new()) {
        return Err("rbs-api's default features must not enable schemars");
    }
    if !enables_schema("schema", features, &mut BTreeSet::new()) {
        return Err("rbs-api's schema feature must enable schemars");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use super::*;

    fn manifest(optional: bool, defaults: &str) -> Value {
        format!(
            "[dependencies]\nschemars = {{ version = '1', optional = {optional} }}\n\
             [features]\nschema = ['dep:schemars']\nhelper = ['schema']\ndefault = [{defaults}]"
        )
        .parse()
        .unwrap()
    }

    #[test]
    fn schema_dependency_must_be_optional_and_off_by_default() {
        assert!(check(&manifest(true, "")).is_ok());
        assert!(check(&manifest(false, "")).is_err());
        assert!(check(&manifest(true, "'schema'")).is_err());
        assert!(check(&manifest(true, "'helper'")).is_err());
    }
}
