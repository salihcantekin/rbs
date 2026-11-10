//! JSON round trip of every request, response and event type, and the JSON-RPC 2.0
//! envelope (ADR-0044 decisions 3 and 7).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::BTreeMap;
use std::fmt::Debug;

use rbs_api::codec::{Codec, JsonCodec, RequestId};
use rbs_api::event::{Event, EventPayload, Topic};
use rbs_api::request::*;
use rbs_api::types::*;
use rbs_api::{ApiError, ErrorKind, Scope, ScopeSet};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) {
    let text = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&text).unwrap();
    assert_eq!(&back, value, "{text}");
}

fn canvas() -> Canvas {
    Canvas {
        width: 1920,
        height: 1080,
        fps_num: 30,
        fps_den: 1,
    }
}

fn encoding() -> Encoding {
    Encoding {
        encoder: None,
        rate_control: RateMode::Cbr,
        video_kbps: 6_000,
        max_video_kbps: None,
        quality: 23,
        keyframe_seconds: 2,
        profile: Some("high".into()),
        preset: Preset::Balanced,
        b_frames: 0,
        cbr_padding: true,
        audio_kbps: 160,
    }
}

fn stream_settings() -> StreamSettings {
    StreamSettings {
        protocol: Some(StreamProtocol::Rtmp),
        key_saved: true,
        reconnect: true,
        srt_latency_ms: None,
        encoding: encoding(),
        auto_record: true,
    }
}

fn profile() -> RecordingProfile {
    RecordingProfile {
        id: "default".into(),
        name: "Default".into(),
        folder: "/home/user/Videos".into(),
        container: Container::Mkv,
        encoder: Some("nvenc".into()),
        rate_control: RateMode::Vbr,
        video_kbps: 12_000,
        max_video_kbps: Some(18_000),
        quality: 23,
        keyframe_seconds: 2,
        profile: Some("high".into()),
        preset: Preset::Balanced,
        b_frames: 2,
        cbr_padding: false,
        audio_kbps: 160,
    }
}

fn source() -> SourceInfo {
    SourceInfo {
        id: 3,
        name: "Camera".into(),
        kind: Some(SourceKind::Camera),
        status: SourceStatus::Failed {
            reason: "unplugged".into(),
        },
        item: Some(7),
        layout: Some(ItemLayout {
            position: [1400.0, 780.0],
            size: [480.0, 270.0],
            rotation: 0.0,
            crop: [0, 0, 0, 0],
            opacity: 1.0,
            style: style(),
        }),
        picture_size: Some([1920, 1080]),
        visible: false,
        locked: true,
        volume: 0.5,
        muted: true,
        mixer_hidden: true,
        filters: vec![FilterInfo {
            id: 2,
            type_id: "org.rbs.filter.chroma-key".into(),
            enabled: true,
            settings: [
                ("key_color".to_owned(), serde_json::json!("#00ff00")),
                ("similarity".to_owned(), serde_json::json!(0.4)),
            ]
            .into_iter()
            .collect(),
        }],
        sync_offset_ms: -40,
    }
}

fn scenes() -> SceneList {
    SceneList {
        scenes: vec![
            SceneInfo {
                id: 0,
                name: "Talk".into(),
            },
            SceneInfo {
                id: 2,
                name: "Game".into(),
            },
        ],
        program: 2,
        preview: Some(2),
        master: Master {
            volume: 0.5,
            muted: true,
        },
        sources: vec![SourceEntry {
            id: 3,
            name: "Camera".into(),
            kind: Some(SourceKind::Camera),
            global: true,
        }],
    }
}

fn style() -> ItemStyle {
    ItemStyle {
        shape: ShapeKind::Circle,
        radius: 0.0,
        border: Some(StyleBorder {
            width: 6.0,
            color: "#ffffffff".into(),
        }),
        shadow: None,
    }
}

/// One of every request.
fn every_request() -> Vec<Request> {
    let c = Request::Command;
    vec![
        Request::Hello(Hello {
            protocols: vec!["1.0".into()],
            client: ClientInfo::new("deck", "2.1"),
        }),
        Request::Authenticate(Authenticate {
            token: "rbs1_x".into(),
        }),
        Request::Describe,
        Request::Unsubscribe(Unsubscribe {
            topics: vec![Topic::AudioLevels],
        }),
        c(Command::StudioGet),
        c(Command::SourceList),
        c(Command::SourceTargets(TargetsParams {
            kind: SourceKind::Window,
        })),
        c(Command::SourceAdd(AddSource {
            kind: SourceKind::Screen,
            adapter: "wgc".into(),
            target: "display-1".into(),
            name: None,
            scene: None,
            global: true,
        })),
        c(Command::SourceDelete(SourceRef { source: 4 })),
        c(Command::ItemSetStyle(SetItemStyle {
            item: 7,
            style: style(),
        })),
        c(Command::ItemSetCrop(SetItemCrop {
            item: 7,
            crop: [760, 180, 760, 180],
            keep_scale: false,
        })),
        c(Command::ItemFlip(FlipItem {
            item: 7,
            axis: Axis::Vertical,
        })),
        c(Command::ItemRotate(RotateItem {
            item: 7,
            degrees: -90,
        })),
        c(Command::ItemFit(FitItem {
            item: 7,
            mode: FitKind::Fill,
        })),
        c(Command::ItemAlign(AlignItem {
            item: 7,
            axis: Some(AlignAxis::Horizontal),
        })),
        c(Command::ItemResetTransform(ResetItemTransform { item: 7 })),
        c(Command::SceneList),
        c(Command::SceneAdd(AddScene { name: None })),
        c(Command::SceneRemove(SceneRef { scene: 2 })),
        c(Command::SceneRename(RenameScene {
            scene: 2,
            name: "Game".into(),
        })),
        c(Command::SceneSwitch(SceneRef { scene: 0 })),
        c(Command::SourceRename(RenameSource {
            source: 3,
            name: "Webcam".into(),
        })),
        c(Command::SourceSetVolume(SetSourceVolume {
            source: 3,
            volume: 0.25,
        })),
        c(Command::SourceSetMuted(SetSourceMuted {
            source: 3,
            muted: true,
        })),
        c(Command::SourceSetMixerHidden(SetSourceMixerHidden {
            source: 3,
            hidden: true,
        })),
        c(Command::CanvasSetMasterVolume(SetMasterVolume {
            volume: 0.5,
        })),
        c(Command::CanvasSetMasterMuted(SetMasterMuted {
            muted: true,
        })),
        c(Command::ItemAdd(AddItem {
            source: 3,
            scene: None,
        })),
        c(Command::SceneSources(SceneRef { scene: 2 })),
        c(Command::CanvasSetPreview(SetPreview { scene: Some(2) })),
        c(Command::CanvasTransition),
        c(Command::ItemRemove(ItemRef { item: 7 })),
        c(Command::GlobalList),
        c(Command::GlobalAdd(AddGlobal { source: 3 })),
        c(Command::GlobalRemove(RemoveGlobal {
            source: 3,
            keep: true,
        })),
        c(Command::ItemSetVisible(SetItemVisible {
            item: 7,
            visible: false,
        })),
        c(Command::ItemSetLocked(SetItemLocked {
            item: 7,
            locked: true,
        })),
        c(Command::EditUndo),
        c(Command::EditRedo),
        c(Command::OutputProfiles),
        c(Command::RecordStart(RecordStart {
            profile: "default".into(),
        })),
        c(Command::RecordStartToFile(RecordStartToFile {
            profile: "default".into(),
            path: "/videos/a.mkv".into(),
        })),
        c(Command::RecordStop),
        c(Command::RecordStatus),
        c(Command::StreamStart),
        c(Command::StreamStop),
        c(Command::StreamStatus),
        c(Command::VcamStatus),
        c(Command::VcamStart),
        c(Command::VcamStop),
        c(Command::SettingsGet),
        c(Command::ProfileList),
        c(Command::ProfileSwitch(EntryRef {
            id: "default".into(),
        })),
        c(Command::CollectionList),
        c(Command::CollectionSwitch(EntryRef {
            id: "talk-show".into(),
        })),
        c(Command::SettingsSet(SettingsPatch {
            profiles: Some(vec![profile()]),
            canvas: Some(canvas()),
            stream: Some(StreamSettingsPatch {
                reconnect: false,
                srt_latency_ms: Some(500),
                encoding: encoding(),
                auto_record: Some(true),
            }),
        })),
        c(Command::EventsSubscribe(Subscribe {
            topics: vec![Topic::RecordState, Topic::AudioLevels],
            rates: BTreeMap::from([(Topic::AudioLevels, 20)]),
        })),
        c(Command::TokensList),
        c(Command::TokensCreate(CreateToken {
            name: "Stream Deck".into(),
            scopes: ScopeSet::PANEL,
        })),
        c(Command::TokensRevoke(TokenRef {
            name: "Stream Deck".into(),
        })),
        c(Command::FilterTypes(FilterTypesParams::default())),
        c(Command::FilterAdd(AddFilter {
            source: 3,
            type_id: "org.rbs.filter.blur".into(),
            settings: Some(BTreeMap::from([("radius".to_owned(), json!(4.0))])),
        })),
        c(Command::FilterRemove(FilterRef { filter: 2 })),
        c(Command::FilterMove(MoveFilter {
            filter: 2,
            index: 0,
        })),
        c(Command::FilterSetEnabled(SetFilterEnabled {
            filter: 2,
            enabled: false,
        })),
        c(Command::FilterSetSettings(SetFilterSettings {
            filter: 2,
            settings: BTreeMap::from([("key_color".to_owned(), json!("#0000ff"))]),
        })),
        c(Command::SourceSetSyncOffset(SetSourceSyncOffset {
            source: 3,
            offset_ms: -40,
        })),
        c(Command::SourceRetarget(RetargetSource {
            source: 3,
            adapter: "org.rbs.capture.test".to_owned(),
            target: "window:7".to_owned(),
        })),
        c(Command::SourceReload(ReloadSource { source: 3 })),
        c(Command::SourceGetSettings(GetSourceSettings { source: 3 })),
        c(Command::SourceSetSettings(SetSourceSettings {
            source: 3,
            settings: [
                ("fps".to_owned(), serde_json::json!(60)),
                ("cursor".to_owned(), serde_json::Value::Null),
            ]
            .into_iter()
            .collect(),
        })),
        c(Command::SettingsRecentTargets(TargetsParams {
            kind: SourceKind::Window,
        })),
        c(Command::SettingsClearRecentTargets(TargetsParams {
            kind: SourceKind::Window,
        })),
    ]
}

#[test]
fn every_request_round_trips_and_every_method_is_covered() {
    let all = every_request();
    for r in &all {
        round_trip(r);
    }
    let mut names: Vec<_> = all.iter().map(Request::method).collect();
    names.sort_unstable();
    let mut methods: Vec<_> = METHODS.iter().map(|m| m.name).collect();
    methods.sort_unstable();
    assert_eq!(names, methods);
}

#[test]
fn filter_types_takes_a_domain_and_defaults_to_video() {
    for (params, want) in [
        (json!({}), FilterDomains::Video),
        (json!({"domain": "audio"}), FilterDomains::Audio),
        (json!({"domain": "all"}), FilterDomains::All),
    ] {
        let p: FilterTypesParams = serde_json::from_value(params).unwrap();
        assert_eq!(p.domain, want);
    }
    round_trip(&Request::Command(Command::FilterTypes(FilterTypesParams {
        domain: FilterDomains::Audio,
    })));
}

#[test]
fn every_request_decodes_from_json_rpc() {
    for (i, r) in every_request().into_iter().enumerate() {
        let wire = serde_json::to_value(&r).unwrap();
        let text = json!({
            "jsonrpc": "2.0",
            "id": i,
            "method": wire["method"],
            "params": wire["params"],
        })
        .to_string();
        let incoming = JsonCodec.decode(&text).unwrap();
        assert_eq!(incoming.id, RequestId::Number(i as i64));
        assert_eq!(incoming.request, r);
    }
}

#[test]
fn every_result_and_event_type_round_trips() {
    let seq = SeqMap::from([("record.state".to_owned(), 4)]);
    round_trip(&Library {
        entries: vec![LibraryEntry {
            id: "talk-show".into(),
            name: "Talk show".into(),
        }],
        current: "talk-show".into(),
        pending: true,
    });
    round_trip(&FilterTypeList {
        types: vec![
            FilterTypeInfo {
                type_id: "org.rbs.filter.blur".into(),
                domain: "video".into(),
                name: "Blur".into(),
                fields: vec![FilterField {
                    key: "radius".into(),
                    label: "Radius".into(),
                    kind: "number".into(),
                    help: None,
                    curve: None,
                    options: None,
                    min: Some(0.0),
                    max: Some(64.0),
                    step: Some(1.0),
                    unit: Some("px".into()),
                    default: json!(8.0),
                }],
            },
            FilterTypeInfo {
                type_id: "org.example.audio-filter.test".into(),
                domain: "audio".into(),
                name: "Test".into(),
                fields: vec![
                    FilterField {
                        key: "attack".into(),
                        label: "Attack".into(),
                        kind: "number".into(),
                        help: Some("How fast it opens.".into()),
                        curve: Some("log".into()),
                        options: None,
                        min: Some(0.1),
                        max: Some(100.0),
                        step: Some(0.1),
                        unit: Some("ms".into()),
                        default: json!(5.0),
                    },
                    FilterField {
                        key: "mode".into(),
                        label: "Mode".into(),
                        kind: "choice".into(),
                        help: None,
                        curve: None,
                        options: Some(vec![FilterOption {
                            key: "soft".into(),
                            label: "Soft".into(),
                        }]),
                        min: None,
                        max: None,
                        step: None,
                        unit: None,
                        default: json!("soft"),
                    },
                ],
            },
        ],
    });
    round_trip(&FilterAdded { filter: 4 });
    round_trip(&HelloResult {
        protocol: "1.0".into(),
        instance: "ab".into(),
        scopes: Some(ScopeSet::ALL),
    });
    round_trip(&AuthResult {
        scopes: ScopeSet::PANEL,
        server: ClientInfo::new("rbs", "0.0.1"),
    });
    round_trip(&Description {
        protocol: "1.0".into(),
        methods: vec![MethodInfoOwned {
            name: "studio.get".into(),
            scope: Some(rbs_api::Scope::Read),
        }],
        topics: Topic::ALL.to_vec(),
    });
    round_trip(&Snapshot {
        seq: seq.clone(),
        data: StudioInfo {
            canvas: canvas(),
            kinds: vec![SourceKind::Screen, SourceKind::ApplicationAudio],
            encoders: vec![EncoderInfo {
                id: "nvenc".into(),
                name: "NVIDIA NVENC".into(),
                rate_controls: vec!["cbr".into()],
                h264_profiles: vec!["high".into()],
                b_frames_max: 4,
            }],
            cannot_record: None,
        },
    });
    round_trip(&Snapshot {
        seq: seq.clone(),
        data: vec![source()],
    });
    round_trip(&TargetList {
        kind: SourceKind::Screen,
        targets: vec![CaptureTarget {
            name: "Display 1".into(),
            adapter: "wgc".into(),
            target: "1".into(),
            is_default: true,
            size: Some(Dimensions {
                width: 2560,
                height: 1440,
            }),
        }],
    });
    round_trip(&SourceAdded { source: 9 });
    round_trip(&TokenList {
        tokens: vec![TokenInfo {
            name: "Stream Deck".into(),
            scopes: ScopeSet::PANEL,
        }],
    });
    let created = TokenCreated {
        name: "Stream Deck".into(),
        scopes: ScopeSet::PANEL,
        token: format!("rbs1_{}", "0".repeat(64)),
    };
    round_trip(&created);
    assert!(!format!("{created:?}").contains("rbs1_"));
    round_trip(&ProfileList {
        profiles: vec![OutputProfile {
            id: "default".into(),
            name: "Default".into(),
            container: Container::Mp4,
            encoder: None,
            rate_control: RateMode::ConstantQuality,
            video_kbps: 12_000,
            audio_kbps: 160,
        }],
    });
    round_trip(&RecordStarted {
        video_encoder: "NVENC".into(),
        audio_encoder: Some("MF AAC".into()),
    });
    round_trip(&Snapshot {
        seq: seq.clone(),
        data: RecordState::Idle {
            last: Some(RecordingSummary {
                failure: Some("disk full".into()),
                frames: 100,
                dropped: 2,
            }),
        },
    });
    round_trip(&Snapshot {
        seq: seq.clone(),
        data: Settings {
            profiles: vec![profile()],
            canvas: canvas(),
            stream: Some(stream_settings()),
        },
    });
    round_trip(&StreamStarted {
        video_encoder: "VideoToolbox".into(),
        audio_encoder: None,
    });
    round_trip(&Snapshot {
        seq: seq.clone(),
        data: StreamState::Idle {
            last: Some(StreamSummary {
                failure: Some("the server closed the connection".into()),
                frames: 1800,
                dropped: 3,
                connects: 2,
            }),
        },
    });
    round_trip(&Snapshot {
        seq: seq.clone(),
        data: vcam_status(),
    });
    round_trip(&Subscribed { seq });
    round_trip(&Empty {});
    round_trip(&ApiError::new(ErrorKind::UndoConflict, "x"));
    for payload in [
        EventPayload::SourceChanged(vec![source()]),
        EventPayload::SceneChanged(scenes()),
        EventPayload::RecordState(RecordState::Recording {
            profile: Some("default".into()),
            video_encoder: "VT".into(),
            audio_encoder: None,
        }),
        EventPayload::CanvasChanged(canvas()),
        EventPayload::SettingsChanged(Settings {
            profiles: vec![],
            canvas: canvas(),
            stream: None,
        }),
        EventPayload::AudioLevels(vec![SourceLevels {
            source: 2,
            peak_db: vec![-6.0, -7.5],
            rms_db: vec![-18.0, -19.25],
        }]),
        EventPayload::StreamState(StreamState::Live {
            protocol: StreamProtocol::Srt,
            connected: true,
            connects: 1,
            reconnect_attempts: 0,
            video_encoder: "NVENC".into(),
            audio_encoder: Some("MF AAC".into()),
        }),
        EventPayload::VcamChanged(VcamStatus {
            frames: None,
            ..vcam_status()
        }),
    ] {
        round_trip(&Event {
            object: None,
            seq: 42,
            payload,
        });
    }
}

fn vcam_status() -> VcamStatus {
    VcamStatus {
        state: Some(VcamInstall::Added),
        reason: None,
        sending: true,
        width: 1280,
        height: 720,
        rate: 30,
        failure: None,
        frames: Some(VcamFrames {
            sent: 300,
            consumer_absent: 12,
            ..VcamFrames::default()
        }),
    }
}

#[test]
fn vcam_status_wire_form() {
    let v = serde_json::to_value(VcamStatus {
        state: Some(VcamInstall::NeedsUpdate),
        reason: Some("Add the camera again to update it".into()),
        frames: None,
        sending: false,
        ..vcam_status()
    })
    .unwrap();
    assert_eq!(v["state"], "needs_update");
    assert_eq!(v["reason"], "Add the camera again to update it");
    assert_eq!(v["frames"], Value::Null);
    let unknown = serde_json::to_value(VcamStatus {
        state: None,
        ..vcam_status()
    })
    .unwrap();
    assert_eq!(unknown["state"], Value::Null);
    assert!(unknown.get("reason").is_none());
    assert_eq!(unknown["frames"]["sent"], 300);
}

#[test]
fn events_are_json_rpc_notifications() {
    let event = Event {
        object: None,
        seq: 42,
        payload: EventPayload::RecordState(RecordState::Idle { last: None }),
    };
    let v: Value = serde_json::from_str(&JsonCodec.encode_event(&event)).unwrap();
    assert_eq!(
        v,
        json!({
            "jsonrpc": "2.0",
            "method": "event",
            "params": {"topic": "record.state", "seq": 42, "data": {"state": "idle", "last": null}},
        })
    );
}

#[test]
fn responses_carry_result_or_typed_error() {
    let id = RequestId::Text("a".into());
    let ok = JsonCodec.encode_response(Some(&id), &Ok(Response::Done(Empty {})));
    assert_eq!(
        serde_json::from_str::<Value>(&ok).unwrap(),
        json!({"jsonrpc": "2.0", "id": "a", "result": {}})
    );
    let err = JsonCodec.encode_response(
        None,
        &Err(ApiError::new(ErrorKind::ScopeDenied, "needs scope admin")),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&err).unwrap(),
        json!({"jsonrpc": "2.0", "id": null, "error": {
            "code": -32010, "message": "needs scope admin", "data": {"kind": "scope_denied"}}})
    );
}

#[test]
fn malformed_messages_get_typed_errors() {
    let kind = |text: &str| JsonCodec.decode(text).unwrap_err().error.kind;
    assert_eq!(kind("{"), ErrorKind::ParseError);
    assert_eq!(
        kind(r#"[{"jsonrpc":"2.0","id":1,"method":"studio.get"}]"#),
        ErrorKind::BatchUnsupported
    );
    assert_eq!(kind(r#""text""#), ErrorKind::InvalidRequest);
    assert_eq!(
        kind(r#"{"jsonrpc":"2.0","method":"studio.get"}"#),
        ErrorKind::InvalidRequest
    );
    assert_eq!(
        kind(r#"{"jsonrpc":"1.0","id":1,"method":"studio.get"}"#),
        ErrorKind::InvalidRequest
    );
    assert_eq!(
        kind(r#"{"jsonrpc":"2.0","id":1,"method":"studio.explode"}"#),
        ErrorKind::MethodNotFound
    );
    assert_eq!(
        kind(r#"{"jsonrpc":"2.0","id":1,"method":"source.delete","params":{"source":"x"}}"#),
        ErrorKind::InvalidParams
    );
    assert_eq!(
        kind(r#"{"jsonrpc":"2.0","id":1,"method":"studio.get","params":[1]}"#),
        ErrorKind::InvalidParams
    );
    let m = JsonCodec
        .decode(r#"{"jsonrpc":"2.0","id":"q","method":"nope"}"#)
        .unwrap_err();
    assert_eq!(m.id, Some(RequestId::Text("q".into())));
}

#[test]
fn methods_without_params_accept_missing_or_null_params() {
    for params in ["", r#","params":null"#, r#","params":{}"#] {
        let text = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"studio.get"{params}}}"#);
        assert_eq!(
            JsonCodec.decode(&text).unwrap().request,
            Request::Command(Command::StudioGet)
        );
    }
}

#[test]
fn authenticate_debug_hides_the_token() {
    let a = Authenticate {
        token: "rbs1_secret".into(),
    };
    assert!(!format!("{a:?}").contains("secret"));
}

#[test]
fn stream_state_has_a_stable_wire_form() {
    let live = StreamState::Live {
        protocol: StreamProtocol::Rtmp,
        connected: false,
        connects: 0,
        reconnect_attempts: 1,
        video_encoder: "VT".into(),
        audio_encoder: None,
    };
    assert_eq!(
        serde_json::to_value(&live).unwrap(),
        json!({
            "state": "live",
            "protocol": "rtmp",
            "connected": false,
            "connects": 0,
            "reconnect_attempts": 1,
            "video_encoder": "VT",
            "audio_encoder": null,
        })
    );
    let event = Event {
        object: None,
        seq: 3,
        payload: EventPayload::StreamState(StreamState::Idle { last: None }),
    };
    let v: Value = serde_json::from_str(&JsonCodec.encode_event(&event)).unwrap();
    assert_eq!(
        v["params"],
        json!({"topic": "stream.state", "seq": 3, "data": {"state": "idle", "last": null}})
    );
    assert_eq!(
        rbs_api::request::method_scope("stream.start"),
        Some(Some(Scope::Control))
    );
    assert_eq!(
        rbs_api::request::method_scope("stream.stop"),
        Some(Some(Scope::Control))
    );
    assert_eq!(
        rbs_api::request::method_scope("stream.status"),
        Some(Some(Scope::Read))
    );
    assert_eq!(
        rbs_api::request::topic_scope(Topic::StreamState),
        Scope::Read
    );
}

/// Protocol 1.2 added the stream settings: a 1.1 `settings.set` without them still
/// parses, settings without them leave the field out, and the settings `settings.get`
/// returned can be sent back as the patch (the read-only fields are ignored).
#[test]
fn stream_settings_are_a_minor_addition() {
    let old: SettingsPatch = serde_json::from_value(json!({"canvas": null})).unwrap();
    assert_eq!(old.stream, None);
    let text = serde_json::to_value(Settings {
        profiles: vec![],
        canvas: canvas(),
        stream: None,
    })
    .unwrap();
    assert!(text.get("stream").is_none(), "{text}");
    let got = serde_json::to_value(stream_settings()).unwrap();
    assert!(got.get("server").is_none(), "{got}");
    let back: StreamSettingsPatch = serde_json::from_value(got).unwrap();
    assert_eq!(
        back,
        StreamSettingsPatch {
            reconnect: true,
            srt_latency_ms: None,
            encoding: encoding(),
            auto_record: Some(true),
        }
    );
    // Protocol 1.11 added `auto_record`: a 1.2 server's settings read as off, and a
    // 1.2 client's patch leaves it as it is.
    let mut old = serde_json::to_value(stream_settings()).unwrap();
    old.as_object_mut().unwrap().remove("auto_record");
    let read: StreamSettings = serde_json::from_value(old.clone()).unwrap();
    assert!(!read.auto_record);
    let patch: StreamSettingsPatch = serde_json::from_value(old).unwrap();
    assert_eq!(patch.auto_record, None);
    let text = serde_json::to_value(&patch).unwrap();
    assert!(text.get("auto_record").is_none(), "{text}");
}

/// Protocol 1.3 added item layouts and `item.set_style` (ADR-0053): a 1.2 source row
/// without them still parses, `{}` is the plain style, unknown style keys are refused,
/// and colours parse as `#rrggbb` or `#rrggbbaa`.
#[test]
fn item_styles_are_a_minor_addition() {
    let old: SourceInfo = serde_json::from_value(json!({
        "id": 1, "name": "Screen", "kind": "screen", "status": {"state": "running"}
    }))
    .unwrap();
    assert_eq!((old.item, old.layout), (None, None));
    let plain: ItemStyle = serde_json::from_value(json!({})).unwrap();
    assert_eq!(plain, ItemStyle::default());
    assert!(serde_json::from_value::<ItemStyle>(json!({"glow": 1})).is_err());
    assert!(serde_json::from_value::<ItemStyle>(json!({"shape": "star"})).is_err());
    let set: Request = serde_json::from_value(json!({
        "method": "item.set_style",
        "params": {"item": 3, "style": {"shape": "rectangle", "radius": 24.0}}
    }))
    .unwrap();
    assert_eq!(set.scope(), Some(Scope::Control));
    assert_eq!(parse_color("#FF0080"), Some([255, 0, 128, 255]));
    assert_eq!(parse_color("#ff008080"), Some([255, 0, 128, 128]));
    for bad in ["ff0080", "#ff008", "#gg0080", "#ff00800", "red"] {
        assert_eq!(parse_color(bad), None, "{bad}");
    }
    assert_eq!(format_color([255, 0, 128, 128]), "#ff008080");
}

#[test]
fn set_crop_keeps_the_scale_unless_told_and_refuses_unknown_keys() {
    let p: SetItemCrop = serde_json::from_value(json!({"item": 7, "crop": [1, 2, 3, 4]})).unwrap();
    assert!(p.keep_scale);
    assert_eq!(p.crop, [1, 2, 3, 4]);
    assert!(
        serde_json::from_value::<SetItemCrop>(json!({"item": 7, "crop": [0, 0, 0, 0], "x": 1}))
            .is_err()
    );
    assert!(serde_json::from_value::<SetItemCrop>(json!({"item": 7, "crop": [0, 0, 0]})).is_err());
}

#[test]
fn scenes_lock_and_volume_are_a_minor_addition() {
    // A 1.5 row reads as shown, unlocked, at full volume, not muted.
    let old: SourceInfo = serde_json::from_value(json!({
        "id": 1, "name": "Mic", "kind": "audio_input", "status": {"state": "running"}
    }))
    .unwrap();
    assert!(old.visible && !old.locked && !old.muted);
    assert!((old.volume - 1.0).abs() < f32::EPSILON);
    round_trip(&source());
    round_trip(&Snapshot {
        seq: SeqMap::new(),
        data: scenes(),
    });
    round_trip(&SceneAdded { scene: 4 });
    round_trip(&ItemAdded { item: 9 });
    let add: Request =
        serde_json::from_value(json!({"method": "scene.add", "params": {}})).unwrap();
    assert_eq!(
        add,
        Request::Command(Command::SceneAdd(AddScene { name: None }))
    );
    assert_eq!(add.scope(), Some(Scope::Control));
    let list: Request =
        serde_json::from_value(json!({"method": "scene.list", "params": {}})).unwrap();
    assert_eq!(list.scope(), Some(Scope::Read));
    assert!(
        serde_json::from_value::<SetItemLocked>(json!({"item": 1, "locked": true, "x": 0}))
            .is_err()
    );
    assert!(serde_json::from_value::<SetSourceVolume>(json!({"source": 1})).is_err());
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
    assert_eq!(cut, Request::Command(Command::CanvasTransition));
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
fn global_sources_and_item_remove_have_a_stable_wire_form() {
    // Protocol 1.10 (ADR-0065).
    let remove: Request =
        serde_json::from_value(json!({"method": "item.remove", "params": {"item": 4}})).unwrap();
    assert_eq!(
        remove,
        Request::Command(Command::ItemRemove(ItemRef { item: 4 }))
    );
    assert_eq!(remove.scope(), Some(Scope::Control));
    let list: Request =
        serde_json::from_value(json!({"method": "global.list", "params": {}})).unwrap();
    assert_eq!(list.scope(), Some(Scope::Read));
    let off: Request =
        serde_json::from_value(json!({"method": "global.remove", "params": {"source": 2}}))
            .unwrap();
    assert_eq!(
        off,
        Request::Command(Command::GlobalRemove(RemoveGlobal {
            source: 2,
            keep: false
        }))
    );
    assert!(serde_json::from_value::<AddGlobal>(json!({"source": 1, "x": 0})).is_err());
    // `global` is left out when false, so 1.9 answers keep their exact form.
    let entry = SourceEntry {
        id: 1,
        name: "Mic".into(),
        kind: Some(SourceKind::AudioInput),
        global: false,
    };
    assert_eq!(
        serde_json::to_value(&entry).unwrap(),
        json!({"id": 1, "name": "Mic", "kind": "audio_input"})
    );
    round_trip(&ItemRemoved {
        source_deleted: true,
    });
    round_trip(&GlobalSourceList {
        sources: vec![source()],
    });
}

#[test]
fn the_master_is_left_out_of_a_scene_list_while_it_is_the_default() {
    let mut list = scenes();
    list.master = Master::default();
    let json = serde_json::to_string(&list).unwrap();
    assert!(!json.contains("master"), "{json}");
    let back: SceneList = serde_json::from_str(&json).unwrap();
    assert_eq!(back, list);
    // A 1.14 server's answer has no master: readers get the default.
    let old = serde_json::to_string(&list).unwrap();
    assert_eq!(
        serde_json::from_str::<SceneList>(&old).unwrap().master,
        Master::default()
    );
}
