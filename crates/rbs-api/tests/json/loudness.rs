//! The loudness meter's values and settings (protocols 1.58 and 1.59, ADR-0092).

use super::*;

#[test]
fn loudness_values_have_their_wire_form() {
    let on: SetLoudnessEnabled = serde_json::from_value(json!({"on": true})).unwrap();
    assert!(on.on);
    assert!(serde_json::from_value::<SetLoudnessEnabled>(json!({"on": true, "x": 1})).is_err());
    // Off: only `enabled`; values without data are left out, never NaN or infinity.
    let off = serde_json::to_value(LoudnessStatus::default()).unwrap();
    assert_eq!(off, json!({"enabled": false}));
    let status = LoudnessStatus {
        enabled: true,
        momentary: Some(-22.5),
        short_term: Some(-23.0),
        integrated: Some(-23.5),
        range: Some(4.5),
        true_peak_max: Some(-1.5),
        momentary_max: Some(-18.0),
        short_term_max: Some(-20.0),
        target: Some(-14.0),
        true_peak_limit: Some(-1.0),
    };
    let v = serde_json::to_value(status).unwrap();
    assert_eq!(v["integrated"], json!(-23.5));
    assert_eq!(v["true_peak_max"], json!(-1.5));
    assert_eq!(v["target"], json!(-14.0));
    round_trip(&status);
}

/// Protocol 1.59 (ADR-0092 decision 5): `loudness` in the settings is optional both
/// ways, so a 1.58 patch and a 1.58 server's settings read as before.
#[test]
fn loudness_settings_are_a_minor_addition() {
    let old: SettingsPatch = serde_json::from_value(json!({"canvas": null})).unwrap();
    assert_eq!(old.loudness, None);
    let new: SettingsPatch =
        serde_json::from_value(json!({"loudness": {"target": -23.0, "true_peak_limit": -2.0}}))
            .unwrap();
    assert_eq!(
        new.loudness,
        Some(LoudnessSettings {
            target: -23.0,
            true_peak_limit: -2.0
        })
    );
    assert!(
        serde_json::from_value::<SettingsPatch>(json!({"loudness": {"target": -23.0}})).is_err()
    );
}
