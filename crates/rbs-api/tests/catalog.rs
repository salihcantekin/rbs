//! The wire's derived list of methods and the catalog must not drift.

use std::collections::BTreeSet;

use rbs_api::{Request, request::METHODS};
use serde_json::json;

#[test]
fn every_wire_method_is_declared_and_the_engine_catalogue_excludes_ui_methods() {
    // Serde's unknown-variant error includes the names from the derived wire enum,
    // independently of the manually constructed request fixtures.
    let error = serde_json::from_value::<Request>(json!({
        "method": "__catalog_probe__",
        "params": {}
    }))
    .unwrap_err()
    .to_string();
    let expected = error
        .split_once("expected one of ")
        .expect("the wire decoder must enumerate the accepted method names")
        .1;
    let wire: BTreeSet<_> = expected.split('`').skip(1).step_by(2).collect();
    let catalog: BTreeSet<_> = METHODS.iter().map(|m| m.name).collect();
    assert_eq!(catalog.len(), METHODS.len(), "duplicate catalog method");
    let routed: BTreeSet<_> = wire.difference(&catalog).copied().collect();
    assert_eq!(
        routed,
        BTreeSet::from(["ui.command.list", "ui.command.run"])
    );
    assert!(
        routed
            .iter()
            .all(|method| rbs_api::request::UI_METHODS.contains(method))
    );
    assert!(
        catalog
            .iter()
            .all(|method| !rbs_api::request::UI_METHODS.contains(method))
    );
    assert_eq!(
        wire,
        catalog.union(&routed).copied().collect(),
        "wire methods and declarations differ"
    );
}

#[test]
fn published_schema_slice_matches_the_wire_types() {
    use rbs_api::request::{AddScene, Empty, RenameScene, Response, SceneAdded, SceneRef};
    use rbs_api::schema::builtin_schemas;

    let schemas = builtin_schemas().unwrap();
    for (name, pair) in &schemas {
        assert!(METHODS.iter().any(|method| method.name == name));
        assert_eq!(pair.input_schema["type"], "object");
        assert!(pair.output_schema.is_object(), "{name}");
    }
    let add = &schemas["scene.add"].input_schema;
    assert_eq!(add["properties"]["name"]["type"], json!(["string", "null"]));
    assert_eq!(add["additionalProperties"], false);
    for value in [json!({}), json!({"name": null}), json!({"name": "Scene"})] {
        assert!(serde_json::from_value::<AddScene>(value).is_ok());
    }
    assert!(serde_json::from_value::<AddScene>(json!({"other": 1})).is_err());
    for name in ["scene.remove", "scene.rename"] {
        let schema = &schemas[name].input_schema;
        assert_eq!(schema["properties"]["scene"]["minimum"], 0);
        assert_eq!(schema["properties"]["scene"]["maximum"], u32::MAX);
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("scene"))
        );
    }
    for value in [json!(0), json!(u32::MAX)] {
        assert!(serde_json::from_value::<SceneRef>(json!({"scene": value})).is_ok());
    }
    for value in [json!(-1), json!(u64::from(u32::MAX) + 1), json!(1.5)] {
        assert!(serde_json::from_value::<SceneRef>(json!({"scene": value})).is_err());
    }
    assert!(serde_json::from_value::<RenameScene>(json!({"scene": 1})).is_err());
    assert!(serde_json::from_value::<RenameScene>(json!({"scene": 1, "name": "a"})).is_ok());
    assert_eq!(
        serde_json::to_value(Response::SceneAdded(SceneAdded { scene: 7 })).unwrap(),
        json!({"scene": 7})
    );
    assert_eq!(
        serde_json::to_value(Response::Done(Empty {})).unwrap(),
        json!({})
    );
}

#[test]
fn new_catalog_methods_require_typed_schemas() {
    let schemas = rbs_api::schema::builtin_schemas().unwrap();
    let catalog: BTreeSet<_> = METHODS.iter().map(|m| m.name).collect();
    let covered: BTreeSet<_> = schemas.keys().map(String::as_str).collect();
    assert_eq!(
        covered, catalog,
        "every method must provide typed input and output schemas"
    );
}

#[test]
fn browser_input_schema_uses_the_wire_list_for_held_buttons() {
    let schemas = rbs_api::schema::builtin_schemas().unwrap();
    let schema = &schemas["browser.input"].input_schema;
    for (held, accepted) in [
        (json!(["left", "right"]), true),
        (json!([]), true),
        (json!(["invalid"]), false),
        (json!({"left": true}), false),
    ] {
        let value = json!({"source": 1, "events": [{"type": "leave", "held": held}]});
        assert_eq!(jsonschema::is_valid(schema, &value), accepted);
        assert_eq!(
            serde_json::from_value::<rbs_api::request::BrowserInput>(value).is_ok(),
            accepted
        );
    }
}

#[test]
fn clipboard_schema_accepts_both_actual_result_shapes() {
    use rbs_api::{request::Response, types::AttributesClipboard};

    let schemas = rbs_api::schema::builtin_schemas().unwrap();
    let schema = &schemas["source.attributes_clipboard"].output_schema;
    for clipboard in [
        None,
        Some(AttributesClipboard {
            source: 7,
            name: "Camera".into(),
            type_id: "camera".into(),
            groups: vec!["filters".into()],
        }),
    ] {
        let value = serde_json::to_value(Response::AttributesClipboard(clipboard)).unwrap();
        assert!(jsonschema::is_valid(schema, &value));
    }
    assert!(!jsonschema::is_valid(schema, &json!({"source": -1})));
}

#[test]
fn output_commands_describe_single_and_all_canvas_results() {
    use rbs_api::{request::Response, types::OutputsResult};
    let schemas = rbs_api::schema::builtin_schemas().unwrap();
    let single = serde_json::to_value(Response::Done(rbs_api::request::Empty {})).unwrap();
    let all = serde_json::to_value(Response::Outputs(OutputsResult { outputs: vec![] })).unwrap();
    for method in [
        "record.stop",
        "record.pause",
        "record.resume",
        "record.pause_toggle",
        "stream.stop",
        "stream.hold",
        "stream.resume",
        "stream.hold_toggle",
    ] {
        let schema = &schemas[method].output_schema;
        assert!(jsonschema::is_valid(schema, &single), "{method}");
        assert!(jsonschema::is_valid(schema, &all), "{method}");
        assert!(
            !jsonschema::is_valid(schema, &json!({"unexpected": true})),
            "{method}"
        );
    }
}

#[test]
fn result_unions_accept_real_nonempty_values() {
    use rbs_api::{
        error::{ApiError, ErrorKind},
        request::{Empty, ItemAdded, ItemRemoved, Response, StreamStarted},
        types::{OutputCanvas, OutputEntry, OutputOutcome, OutputsResult},
    };
    let schemas = rbs_api::schema::builtin_schemas().unwrap();
    let mut error = ApiError::new(ErrorKind::Failed, "refused");
    error.data = Some(serde_json::Map::from_iter([(
        "reason".into(),
        json!("over_limit"),
    )]));
    let outputs = Response::Outputs(OutputsResult {
        outputs: vec![OutputEntry {
            canvas: OutputCanvas::Vertical,
            kind: None,
            outcome: OutputOutcome::Refused,
            video_encoder: None,
            audio_encoder: None,
            error: Some((&error).into()),
            estimate: None,
            skipped: vec![],
        }],
    });
    for (method, result) in [
        ("record.start", outputs),
        ("global.remove", Response::ItemAdded(ItemAdded { item: 1 })),
        (
            "global.remove",
            Response::ItemRemoved(ItemRemoved {
                source_deleted: true,
                removed_items: vec![2],
                deleted_sources: vec![3],
            }),
        ),
        ("stream.destination.start", Response::Done(Empty {})),
        (
            "stream.destination.start",
            Response::StreamStarted(StreamStarted {
                video_encoder: "encoder".into(),
                audio_encoder: None,
            }),
        ),
    ] {
        let value = serde_json::to_value(result).unwrap();
        assert!(
            jsonschema::is_valid(&schemas[method].output_schema, &value),
            "{method}: {value}"
        );
    }
}

#[test]
fn status_schemas_accept_serialized_main_and_vertical_states() {
    use rbs_api::types::{
        CanvasRecordState, CanvasStreamState, OutputCanvas, RecordState, Snapshot, StreamState,
    };
    let schemas = rbs_api::schema::builtin_schemas().unwrap();
    let seq: rbs_api::types::SeqMap = Default::default();
    let record = RecordState::Idle { last: None };
    let stream = StreamState::idle();
    let cases = [
        (
            "record.status",
            serde_json::to_value(Snapshot {
                seq: seq.clone(),
                data: record.clone(),
            })
            .unwrap(),
        ),
        (
            "record.status",
            serde_json::to_value(Snapshot {
                seq: seq.clone(),
                data: CanvasRecordState {
                    canvas: OutputCanvas::Vertical,
                    state: record,
                },
            })
            .unwrap(),
        ),
        (
            "stream.status",
            serde_json::to_value(Snapshot {
                seq: seq.clone(),
                data: stream.clone(),
            })
            .unwrap(),
        ),
        (
            "stream.status",
            serde_json::to_value(Snapshot {
                seq,
                data: CanvasStreamState {
                    canvas: OutputCanvas::Vertical,
                    state: stream,
                },
            })
            .unwrap(),
        ),
    ];
    for (method, value) in cases {
        assert!(
            jsonschema::is_valid(&schemas[method].output_schema, &value),
            "{method}: {value}"
        );
    }
}

#[test]
fn settings_patch_schema_accepts_missing_and_explicit_null_fields() {
    let schema = &rbs_api::schema::builtin_schemas().unwrap()["settings.set"].input_schema;
    for value in [
        json!({}),
        json!({"vertical_profile": null}),
        json!({"vertical_stream": null}),
    ] {
        assert!(serde_json::from_value::<rbs_api::request::SettingsPatch>(value.clone()).is_ok());
        assert!(jsonschema::is_valid(schema, &value), "{value}");
    }
    assert!(!jsonschema::is_valid(
        schema,
        &json!({"vertical_profile": 4})
    ));
}

#[test]
fn schemas_preserve_nullable_bounds_and_serialized_result_shapes() {
    let schemas = rbs_api::schema::builtin_schemas().unwrap();
    let preview = jsonschema::validator_for(&schemas["canvas.set_preview"].input_schema).unwrap();
    assert!(preview.is_valid(&json!({"scene": null})));
    assert!(preview.is_valid(&json!({"scene": u32::MAX})));
    assert!(!preview.is_valid(&json!({"scene": -1})));
    assert!(!preview.is_valid(&json!({"scene": u64::from(u32::MAX) + 1})));

    let output = jsonschema::validator_for(&schemas["scene.audio.get"].output_schema).unwrap();
    let result = rbs_api::request::SceneLevels {
        scene: 1,
        own: true,
        levels: vec![rbs_api::request::SceneLevelInfo {
            source: 2,
            volume: None,
            muted: None,
        }],
    };
    let serialized = serde_json::to_value(result).unwrap();
    assert!(output.is_valid(&serialized));
    assert_eq!(serialized["levels"][0], json!({"source": 2}));
    assert!(!output.is_valid(&json!({
        "scene": 1, "own": true,
        "levels": [{"source": u64::from(u32::MAX) + 1}]
    })));
}

#[test]
fn schemas_describe_serialized_handshake_and_media_results() {
    let schemas = rbs_api::schema::builtin_schemas().unwrap();
    let hello = rbs_api::request::HelloResult {
        protocol: "1.108".into(),
        instance: "test".into(),
        scopes: Some(rbs_api::ScopeSet::EMPTY.with(rbs_api::Scope::Read)),
    };
    let value = serde_json::to_value(hello).unwrap();
    assert_eq!(value["scopes"], json!(["read"]));
    assert!(jsonschema::is_valid(
        &schemas["session.hello"].output_schema,
        &value
    ));
    let media = rbs_api::request::MediaStateInfo {
        source: 7,
        state: rbs_api::request::MediaPlayState::Paused,
        cursor_ms: 100,
        duration_ms: None,
        index: None,
        count: None,
    };
    let value = serde_json::to_value(media).unwrap();
    assert!(jsonschema::is_valid(
        &schemas["media.pause"].output_schema,
        &value
    ));
    assert!(value.get("duration_ms").is_none());
}

#[cfg(feature = "schema")]
#[test]
fn generated_schemas_match_the_checked_in_snapshot() {
    assert_eq!(
        rbs_api::schema::generate_schemas().unwrap(),
        rbs_api::schema::builtin_schemas().unwrap()
    );
}
