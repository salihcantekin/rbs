//! JSON round trip of every request, response and event type, and the JSON-RPC 2.0
//! envelope (ADR-0044 decisions 3 and 7).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::BTreeMap;
use std::fmt::Debug;

use rbs_api::codec::{Codec, JsonCodec, RequestId};
use rbs_api::event::{Event, EventPayload, Topic};
use rbs_api::request::*;
use rbs_api::types::*;
use rbs_api::{ApiError, ErrorKind, ScopeSet};
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
        })),
        c(Command::SourceDelete(SourceRef { source: 4 })),
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
        c(Command::SettingsGet),
        c(Command::SettingsSet(SettingsPatch {
            profiles: Some(vec![profile()]),
            canvas: Some(canvas()),
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
        },
    });
    round_trip(&Subscribed { seq });
    round_trip(&Empty {});
    round_trip(&ApiError::new(ErrorKind::UndoConflict, "x"));
    for payload in [
        EventPayload::SourceChanged(vec![source()]),
        EventPayload::RecordState(RecordState::Recording {
            profile: Some("default".into()),
            video_encoder: "VT".into(),
            audio_encoder: None,
        }),
        EventPayload::CanvasChanged(canvas()),
        EventPayload::SettingsChanged(Settings {
            profiles: vec![],
            canvas: canvas(),
        }),
        EventPayload::AudioLevels(vec![SourceLevels {
            source: 2,
            peak_db: vec![-6.0, -7.5],
            rms_db: vec![-18.0, -19.25],
        }]),
    ] {
        round_trip(&Event {
            object: None,
            seq: 42,
            payload,
        });
    }
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
