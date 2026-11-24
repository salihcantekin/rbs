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

#[test]
fn a_source_row_names_its_group_only_when_it_has_one() {
    let plain = serde_json::to_value(SourceInfo {
        group: None,
        ..source()
    })
    .unwrap();
    assert!(plain.get("group").is_none());
    let row = serde_json::to_value(SourceInfo {
        group: Some(3),
        ..source()
    })
    .unwrap();
    assert_eq!(row["group"], json!(3));
    let back: SourceInfo = serde_json::from_value(row).unwrap();
    assert_eq!(back.group, Some(3));
    // An older server's row has no group.
    let older: SourceInfo = serde_json::from_value(json!({
        "id": 1, "name": "Mic", "kind": "audio_input", "status": {"state": "running"}, "volume": 1.0
    }))
    .unwrap();
    assert_eq!(older.group, None);
}

#[test]
fn group_requests_read_their_parameters_and_refuse_unknown_ones() {
    let made: Request = serde_json::from_value(json!({
        "method": "group.create", "params": {"scene": 1, "items": [2, 3]}
    }))
    .unwrap();
    assert_eq!(
        made,
        Request::Command(Command::GroupCreate(CreateGroup {
            scene: 1,
            items: vec![2, 3],
            name: None,
        }))
    );
    assert!(
        serde_json::from_value::<Request>(json!({
            "method": "group.remove", "params": {"scene": 1, "group": 1, "extra": 0}
        }))
        .is_err()
    );
    for method in ["group.create", "group.remove", "group.rename"] {
        assert_eq!(method_effect(method), Some(Effect::Edit), "{method}");
    }
    assert_eq!(method_effect("group.list"), Some(Effect::Read));
}

#[test]
fn group_answers_round_trip() {
    round_trip(&GroupCreated {
        group: 2,
        name: "Group 2".into(),
    });
    round_trip(&GroupList {
        scene: 1,
        groups: vec![GroupInfo {
            id: 2,
            name: "Group 2".into(),
            items: vec![5, 4],
        }],
    });
}

/// The `group.*` requests, covered by `every_request_round_trips_and_every_method_is_covered`.
pub(super) fn group_requests() -> Vec<Request> {
    [
        Command::GroupCreate(CreateGroup {
            scene: 2,
            items: vec![4, 5],
            name: Some("Webcam".into()),
        }),
        Command::GroupCreate(CreateGroup {
            scene: 2,
            items: vec![4, 5],
            name: None,
        }),
        Command::GroupRemove(GroupRef { scene: 2, group: 1 }),
        Command::GroupRename(RenameGroup {
            scene: 2,
            group: 1,
            name: "Cam".into(),
        }),
        Command::GroupList(SceneRef { scene: 2 }),
        Command::ItemSetVisible(SetItemVisible {
            item: 7,
            visible: false,
            gesture: 0,
        }),
        Command::ItemSetLocked(SetItemLocked {
            item: 7,
            locked: true,
            gesture: 9,
        }),
        Command::ItemRemove(ItemRef {
            item: 7,
            gesture: 0,
        }),
    ]
    .into_iter()
    .map(Request::Command)
    .collect()
}

#[test]
fn studio_mode_is_a_minor_addition() {
    // A 1.6 scene list and 1.6 requests still read, meaning the program scene.
    let old: SceneList = serde_json::from_value(json!({
        "scenes": [{"id": 0, "name": "Talk"}], "program": 0, "sources": []
    }))
    .unwrap();
    assert_eq!(old.preview, None);
    let add: AddItem = serde_json::from_value(json!({"source": 3})).unwrap();
    assert_eq!(add.scene, None);
    let off: Request =
        serde_json::from_value(json!({"method": "canvas.set_preview", "params": {"scene": null}}))
            .unwrap();
    assert_eq!(
        off,
        Request::Command(Command::CanvasSetPreview(SetPreview { scene: None }))
    );
    assert_eq!(off.scope(), Some(Scope::Control));
    let cut: Request =
        serde_json::from_value(json!({"method": "canvas.transition", "params": {}})).unwrap();
    assert_eq!(
        cut,
        Request::Command(Command::CanvasTransition(TransitionOnce::default()))
    );
    // Protocol 1.81 (ADR-0128): a one-off kind and duration; old requests still parse.
    let fade: Request = serde_json::from_value(json!({
        "method": "canvas.transition",
        "params": {"kind": "fade", "duration_ms": 1000}
    }))
    .unwrap();
    assert_eq!(
        fade,
        Request::Command(Command::CanvasTransition(TransitionOnce {
            kind: Some(TransitionKind::Fade),
            duration_ms: Some(1000),
        }))
    );
    let switch: Request =
        serde_json::from_value(json!({"method": "scene.switch", "params": {"scene": 4}})).unwrap();
    assert_eq!(
        switch,
        Request::Command(Command::SceneSwitch(SwitchScene {
            scene: 4,
            kind: None,
            duration_ms: None,
        }))
    );
    let set: Request = serde_json::from_value(json!({
        "method": "collection.set_transition",
        "params": {"kind": "fade"}
    }))
    .unwrap();
    assert_eq!(set.scope(), Some(Scope::Control));
    // The kind outside studio mode (protocol 1.97, ADR-0153).
    let direct: Request = serde_json::from_value(json!({
        "method": "collection.set_transition",
        "params": {"direct_kind": "cut"}
    }))
    .unwrap();
    assert_eq!(
        direct,
        Request::Command(Command::CollectionSetTransition(SetCollectionTransition {
            direct_kind: Some(TransitionKind::Cut),
            ..Default::default()
        }))
    );
    let mut cut = scenes();
    cut.transition.direct_kind = TransitionKind::Cut;
    let wire = serde_json::to_value(&cut).unwrap();
    assert_eq!(wire["transition"]["direct_kind"], "cut");
    round_trip(&cut);
    // Move, the default, is left out.
    assert!(
        serde_json::to_value(scenes()).unwrap()["transition"]
            .get("direct_kind")
            .is_none()
    );
    assert!(
        serde_json::from_value::<Request>(json!({
            "method": "canvas.transition",
            "params": {"kind": "wipe"}
        }))
        .is_err()
    );
    // A default transition is left out of the scene list.
    let mut plain = scenes();
    plain.transition = Transition::default();
    assert!(
        serde_json::to_value(&plain)
            .unwrap()
            .get("transition")
            .is_none()
    );
    let sources: Request =
        serde_json::from_value(json!({"method": "scene.sources", "params": {"scene": 1}})).unwrap();
    assert_eq!(sources.scope(), Some(Scope::Read));
    assert!(serde_json::from_value::<SetPreview>(json!({"scene": 1, "x": 0})).is_err());
    round_trip(&SceneSourceList {
        scene: 2,
        sources: vec![source()],
    });
}

#[test]
fn a_status_with_missing_words_still_decodes() {
    let bare: SourceStatus = serde_json::from_value(json!({"state": "unavailable"})).unwrap();
    assert_eq!(
        bare,
        SourceStatus::Unavailable {
            fault: SourceFault::Other,
            detail: String::new()
        }
    );
    let held: SourceStatus =
        serde_json::from_value(json!({"state": "paused", "detail": "held"})).unwrap();
    assert_eq!(
        held,
        SourceStatus::Failed {
            reason: "held".into()
        }
    );
}

#[test]
fn item_edits_take_an_optional_gesture() {
    // Protocol 1.105: absent means 0, and 0 is not written.
    let plain: SetItemVisible =
        serde_json::from_value(json!({"item": 1, "visible": false})).unwrap();
    assert_eq!(plain.gesture, 0);
    assert!(
        serde_json::to_value(plain)
            .unwrap()
            .get("gesture")
            .is_none()
    );
    let locked: SetItemLocked =
        serde_json::from_value(json!({"item": 1, "locked": true, "gesture": 5})).unwrap();
    assert_eq!(locked.gesture, 5);
    let removed: ItemRef = serde_json::from_value(json!({"item": 1, "gesture": 6})).unwrap();
    assert_eq!(removed.gesture, 6);
}
