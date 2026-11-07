//! Filter settings in the scene model to and from filter values (spec
//! `video-filters.md`, "Values"; ADR-0062 decisions 2 and 8).

use rbs_filters::{FieldKind, FilterError, FilterType, Value, Values, color_text, parse_color};
use rbs_scene::model::{SettingValue, Settings};

/// The value a stored setting holds for `kind`, or `None` for the wrong kind.
fn value_of(kind: &FieldKind, v: &SettingValue) -> Option<Value> {
    match (kind, v) {
        (FieldKind::Number { .. }, SettingValue::Float(f)) => Some(Value::Number(*f as f32)),
        (FieldKind::Number { .. }, SettingValue::Int(i)) => Some(Value::Number(*i as f32)),
        (FieldKind::Color { .. }, SettingValue::Text(t)) => parse_color(t).map(Value::Color),
        _ => None,
    }
}

/// Strict conversion, for edits: unknown keys, wrong kinds and out-of-range or
/// non-finite numbers are refused; missing keys take their defaults.
pub fn filter_values(ty: &dyn FilterType, settings: &Settings) -> Result<Values, FilterError> {
    let fields = ty.fields();
    let mut pairs = Vec::with_capacity(settings.len());
    for (key, v) in settings {
        let field = fields
            .iter()
            .find(|f| f.key == key)
            .ok_or_else(|| FilterError::UnknownKey(key.clone()))?;
        let value = value_of(&field.kind, v).ok_or(FilterError::WrongKind(field.key))?;
        pairs.push((field.key, value));
    }
    Values::from_pairs(ty, pairs)
}

/// Lenient conversion, for settings read from a document: unknown keys are ignored,
/// numbers clamped into range, wrong kinds and non-finite numbers replaced by the
/// default. Returns the values and the keys it adjusted (unknown keys included).
pub fn filter_values_lenient(ty: &dyn FilterType, settings: &Settings) -> (Values, Vec<String>) {
    let fields = ty.fields();
    let mut adjusted = Vec::new();
    let mut pairs = Vec::with_capacity(settings.len());
    for (key, v) in settings {
        let Some(field) = fields.iter().find(|f| f.key == key) else {
            adjusted.push(key.clone());
            continue;
        };
        let value = match (value_of(&field.kind, v), field.kind) {
            (Some(Value::Number(n)), FieldKind::Number { min, max, .. }) if n.is_finite() => {
                let c = n.clamp(min, max);
                if c != n {
                    adjusted.push(key.clone());
                }
                Value::Number(c)
            }
            (Some(Value::Color(c)), _) => Value::Color(c),
            _ => {
                adjusted.push(key.clone());
                continue;
            }
        };
        pairs.push((field.key, value));
    }
    // Every pair is valid now, so this cannot be refused; defaults if it were.
    let values = Values::from_pairs(ty, pairs).unwrap_or_else(|_| Values::defaults(ty));
    (values, adjusted)
}

/// `v` as the shortest decimal that reads back as the same `f32` (0.1, not
/// 0.10000000149011612), as documents write floats.
fn wide(v: f32) -> f64 {
    format!("{v}").parse().unwrap_or(f64::from(v))
}

/// The settings that store `values`: every field of the type, numbers as floats and
/// colours as `#rrggbb` or `#rrggbbaa`.
pub fn filter_settings(ty: &dyn FilterType, values: &Values) -> Settings {
    values
        .to_pairs(ty)
        .into_iter()
        .map(|(key, v)| {
            let stored = match v {
                Value::Number(n) => SettingValue::Float(wide(n)),
                Value::Color(c) => SettingValue::Text(color_text(c)),
                // `Value` is non-exhaustive; a later kind is stored as text.
                #[allow(unreachable_patterns)]
                other => SettingValue::Text(format!("{other:?}")),
            };
            (key.to_owned(), stored)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use rbs_filters::catalogue::{BLUR, CHROMA_KEY, find};

    fn s(pairs: &[(&str, SettingValue)]) -> Settings {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn strict_conversion_refuses_what_the_type_refuses() {
        let blur = find(BLUR).unwrap();
        let v = filter_values(blur, &s(&[("radius", SettingValue::Int(4))])).unwrap();
        assert_eq!(v.number(0), 4.0);
        assert_eq!(
            filter_values(blur, &s(&[("radius", SettingValue::Float(65.0))])),
            Err(FilterError::OutOfRange("radius"))
        );
        assert_eq!(
            filter_values(blur, &s(&[("size", SettingValue::Float(1.0))])),
            Err(FilterError::UnknownKey("size".to_owned()))
        );
        assert_eq!(
            filter_values(blur, &s(&[("radius", SettingValue::Bool(true))])),
            Err(FilterError::WrongKind("radius"))
        );
        let key = find(CHROMA_KEY).unwrap();
        assert_eq!(
            filter_values(
                key,
                &s(&[("key_color", SettingValue::Text("green".into()))])
            ),
            Err(FilterError::WrongKind("key_color"))
        );
    }

    #[test]
    fn lenient_conversion_repairs_and_reports() {
        let blur = find(BLUR).unwrap();
        let (v, adjusted) = filter_values_lenient(
            blur,
            &s(&[
                ("radius", SettingValue::Float(500.0)),
                ("gone", SettingValue::Bool(true)),
            ]),
        );
        assert_eq!(v.number(0), 64.0);
        assert_eq!(adjusted, vec!["gone".to_owned(), "radius".to_owned()]);
        let (v, adjusted) =
            filter_values_lenient(blur, &s(&[("radius", SettingValue::Float(f64::NAN))]));
        assert_eq!(v, Values::defaults(blur));
        assert_eq!(adjusted, vec!["radius".to_owned()]);
        let (_, none) = filter_values_lenient(blur, &s(&[("radius", SettingValue::Float(3.0))]));
        assert!(none.is_empty());
    }

    #[test]
    fn settings_round_trip_with_every_field() {
        let key = find(CHROMA_KEY).unwrap();
        let v = filter_values(
            key,
            &s(&[("key_color", SettingValue::Text("#0000ff80".into()))]),
        )
        .unwrap();
        let stored = filter_settings(key, &v);
        assert_eq!(stored.len(), key.fields().len());
        assert_eq!(stored["key_color"], SettingValue::Text("#0000ff80".into()));
        assert_eq!(filter_values(key, &stored).unwrap(), v);
    }

    #[test]
    fn numbers_are_stored_as_their_shortest_decimal() {
        let blur = find(BLUR).unwrap();
        let v = Values::from_pairs(blur, [("radius", Value::Number(0.1))]).unwrap();
        let stored = filter_settings(blur, &v);
        assert_eq!(stored["radius"], SettingValue::Float(0.1));
        assert_eq!(filter_values(blur, &stored).unwrap(), v);
    }

    #[test]
    fn the_scene_sync_offset_limit_is_the_audio_inputs_default() {
        let ms = crate::registry::DEFAULT_MAX_AUDIO_OFFSET.as_millis();
        assert_eq!(
            ms,
            u128::from(rbs_scene::model::MAX_SYNC_OFFSET_MS.unsigned_abs())
        );
    }
}
