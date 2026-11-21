//! Source rows of `source.list`: latency fields and live counters.

use super::*;

#[test]
fn filter_latency_fields_are_optional_and_round_trip() {
    // Protocol 1.77 (ADR-0130 decision 9): a source without latent filters, offset or
    // running input looks as it did before.
    let plain = serde_json::to_value(SourceInfo {
        filter_latency_ms: 0.0,
        effective_offset_ms: 0.0,
        uncompensated_ms: 0.0,
        input_late_frames: None,
        input_resyncs: None,
        ..source()
    })
    .unwrap();
    for key in [
        "filter_latency_ms",
        "effective_offset_ms",
        "uncompensated_ms",
        "input_late_frames",
        "input_resyncs",
        "warm_up_ms",
    ] {
        assert!(plain.get(key).is_none(), "{key}");
    }
    let row = serde_json::to_value(SourceInfo {
        uncompensated_ms: 4.0,
        warm_up_ms: Some(640),
        ..source()
    })
    .unwrap();
    assert_eq!(row["filter_latency_ms"], json!(10.0));
    assert_eq!(row["effective_offset_ms"], json!(-30.0));
    assert_eq!(row["uncompensated_ms"], json!(4.0));
    assert_eq!(row["input_late_frames"], json!(96));
    assert_eq!(row["input_resyncs"], json!(1));
    // Protocol 1.98 (ADR-0152 decision 10): the last warm-up.
    assert_eq!(row["warm_up_ms"], json!(640));
    let back: SourceInfo = serde_json::from_value(row).unwrap();
    assert_eq!(back.uncompensated_ms, 4.0);
    assert_eq!(back.warm_up_ms, Some(640));
    // An older server's row reads with zero latency and no counters.
    let older: SourceInfo = serde_json::from_value(json!({
        "id": 1, "name": "Mic", "kind": "audio_input", "status": {"state": "running"}, "volume": 1.0
    }))
    .unwrap();
    assert_eq!(older.filter_latency_ms, 0.0);
    assert_eq!(older.input_resyncs, None);
    assert_eq!(older.warm_up_ms, None);
    // An older server's filter type has no latency.
    let ty: FilterTypeInfo = serde_json::from_value(
        json!({"type": "org.rbs.filter.blur", "name": "Blur", "fields": []}),
    )
    .unwrap();
    assert_eq!(ty.latency_ms, None);
}
