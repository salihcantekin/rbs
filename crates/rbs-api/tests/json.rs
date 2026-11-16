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
        video_codec: VideoCodec::H264,
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
        delivery: Delivery::Video,
        open_shadows: false,
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
        output_size: None,
        hold_picture: HoldPicture::Black,
        hold_scene: Some(4),
        hold_sound: HoldSound::Scene,
    }
}

fn profile() -> RecordingProfile {
    RecordingProfile {
        id: "default".into(),
        name: "Default".into(),
        folder: "/home/user/Videos".into(),
        container: Container::Mkv,
        encoder: Some("nvenc".into()),
        video_codec: VideoCodec::H264,
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
        bit_depth: 16,
        delivery: Delivery::Video,
        open_shadows: false,
        output_size: None,
        editor_project: None,
        markers: Default::default(),
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
        companion_of: Some(2),
        volume: 0.5,
        muted: true,
        mixer_hidden: true,
        record_separately: true,
        record_tracks: Some(rbs_api::types::RecordTracks {
            video: true,
            audio: false,
            video_as: rbs_api::types::PictureForm::Image,
        }),
        monitor: MonitorMode::Only,
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
        glide: None,
        heard: Some(Heard {
            volume: 1.0,
            muted: false,
        }),
    }
}

fn scenes() -> SceneList {
    SceneList {
        scenes: vec![
            SceneInfo {
                id: 0,
                name: "Talk".into(),
                own_audio: true,
            },
            SceneInfo {
                id: 2,
                name: "Game".into(),
                own_audio: false,
            },
        ],
        program: 2,
        preview: Some(2),
        master: Master {
            volume: 0.5,
            muted: true,
        },
        glide: Glide::default(),
        sources: vec![SourceEntry {
            id: 3,
            name: "Camera".into(),
            kind: Some(SourceKind::Camera),
            global: true,
            glide: None,
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
            with_audio: true,
            text: None,
        })),
        c(Command::SourceAdd(AddSource {
            kind: SourceKind::Text,
            adapter: String::new(),
            target: String::new(),
            name: Some("Title".into()),
            scene: Some(2),
            global: false,
            with_audio: false,
            text: Some("Hello".into()),
        })),
        c(Command::SourceSetText(SetText {
            source: 4,
            text: "Bye".into(),
        })),
        c(Command::TextFonts),
        c(Command::SourceSetCompanionAudio(SetCompanionAudio {
            source: 4,
            enabled: true,
        })),
        c(Command::SourceDelete(SourceRef { source: 4 })),
        c(Command::ItemSetStyle(SetItemStyle {
            item: 7,
            style: style(),
            canvas: OutputCanvas::Main,
        })),
        c(Command::ItemSetCrop(SetItemCrop {
            item: 7,
            crop: [760, 180, 760, 180],
            keep_scale: false,
            canvas: OutputCanvas::Vertical,
        })),
        c(Command::ItemFlip(FlipItem {
            item: 7,
            axis: Axis::Vertical,
            canvas: OutputCanvas::Main,
        })),
        c(Command::ItemRotate(RotateItem {
            item: 7,
            degrees: -90,
            canvas: OutputCanvas::Vertical,
        })),
        c(Command::ItemFit(FitItem {
            item: 7,
            mode: FitKind::Fill,
            canvas: OutputCanvas::Vertical,
        })),
        c(Command::ItemAlign(AlignItem {
            item: 7,
            axis: Some(AlignAxis::Horizontal),
            canvas: OutputCanvas::Main,
        })),
        c(Command::ItemResetTransform(ResetItemTransform {
            item: 7,
            canvas: OutputCanvas::Vertical,
        })),
        c(Command::CanvasAutoLayout(CanvasAutoLayout {
            canvas: OutputCanvas::Vertical,
            mode: VerticalLayout::Stack,
            scene: Some(2),
        })),
        c(Command::CanvasSetVerticalSize(SetVerticalSize {
            width: 1440,
            height: 2560,
        })),
        c(Command::SceneList),
        c(Command::SceneAdd(AddScene { name: None })),
        c(Command::SceneRemove(SceneRef { scene: 2 })),
        c(Command::SceneRename(RenameScene {
            scene: 2,
            name: "Game".into(),
        })),
        c(Command::SceneDuplicate(DuplicateScene {
            scene: 2,
            sources: Default::default(),
        })),
        c(Command::SceneDuplicate(DuplicateScene {
            scene: 2,
            sources: DuplicateSources::Copied,
        })),
        c(Command::SourceDuplicate(SourceInScene {
            source: 3,
            scene: Some(2),
            canvas: OutputCanvas::Main,
        })),
        c(Command::SourceReset(ResetSource {
            source: 3,
            scene: None,
            canvas: OutputCanvas::Vertical,
            groups: Some(vec!["crop".into(), "sound".into()]),
        })),
        c(Command::SourceCopyAttributes(SourceInScene {
            source: 3,
            scene: None,
            canvas: OutputCanvas::Main,
        })),
        c(Command::SourceAttributesClipboard),
        c(Command::SourcePasteAttributes(PasteAttributes {
            items: vec![4, 5],
            groups: vec!["video_filters".into()],
            filters: FilterPasteMode::Append,
            canvas: OutputCanvas::Main,
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
        c(Command::CanvasAddVertical(AddVerticalCanvas {
            width: Some(720),
            height: Some(1280),
            follow: Some(false),
            layout: Some(VerticalLayout::Stack),
        })),
        c(Command::CanvasRemoveVertical(Empty {})),
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
        c(Command::ItemMove(MoveItem {
            item: 7,
            direction: LayerDirection::Up,
        })),
        c(Command::SceneMove(MoveScene {
            scene: 2,
            direction: LayerDirection::Down,
        })),
        c(Command::SourceSetActive(SetSourceActive {
            source: 3,
            active: false,
        })),
        c(Command::LookList),
        c(Command::LookApply(ApplyLook {
            source: 3,
            look: "builtin:warm".into(),
        })),
        c(Command::SourceAddSample(AddSample {
            sample: "aurora".into(),
        })),
        c(Command::ItemSetBox(SetItemBox {
            source: 3,
            position: [10.0, 20.0],
            size: [640.0, 360.0],
            gesture: 5,
            seq: 2,
            canvas: OutputCanvas::Vertical,
        })),
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
        c(Command::ItemSetShowFor(SetItemShowFor {
            item: 7,
            show_for_ms: Some(5_000),
        })),
        c(Command::ItemShow(ShowItem { item: 7 })),
        c(Command::EditUndo),
        c(Command::EditRedo),
        c(Command::OutputProfiles),
        c(Command::RecordStart(RecordStart {
            profile: "default".into(),
            canvas: None,
            iso: Some(false),
            acknowledge_heavy: Some(true),
            on_hardware_limit: Some(HardwareLimitPolicy::Skip),
        })),
        c(Command::RecordStart(RecordStart {
            profile: "default".into(),
            canvas: Some(OutputTarget::Vertical),
            iso: None,
            acknowledge_heavy: None,
            on_hardware_limit: None,
        })),
        c(Command::RecordEstimate(RecordStart {
            profile: "default".into(),
            canvas: Some(OutputTarget::All),
            iso: None,
            acknowledge_heavy: None,
            on_hardware_limit: None,
        })),
        c(Command::RecordStartToFile(RecordStartToFile {
            profile: "default".into(),
            path: "/videos/a.mkv".into(),
            canvas: None,
        })),
        c(Command::RecordStop(OnCanvases::default())),
        c(Command::RecordStatus(OnCanvas::default())),
        c(Command::StreamStart(OnCanvases::default())),
        c(Command::StreamStop(OnCanvases {
            canvas: Some(OutputTarget::Vertical),
        })),
        c(Command::RecordStop(OnCanvases {
            canvas: Some(OutputTarget::All),
        })),
        c(Command::RecordPause(OnCanvases::default())),
        c(Command::RecordResume(OnCanvases {
            canvas: Some(OutputTarget::Vertical),
        })),
        c(Command::RecordPauseToggle(OnCanvases {
            canvas: Some(OutputTarget::All),
        })),
        c(Command::StreamHold(StreamHold {
            canvas: None,
            picture: Some(HoldPicture::Black),
            scene: None,
            sound: None,
        })),
        c(Command::StreamResume(OnCanvases {
            canvas: Some(OutputTarget::All),
        })),
        c(Command::StreamHoldToggle(StreamHold::default())),
        c(Command::OutputBreakToggle),
        c(Command::OutputSetWithVertical(SetWithVertical {
            kind: OutputKind::Stream,
            on: false,
        })),
        c(Command::StreamStatus(OnCanvas::default())),
        c(Command::ReplayStatus),
        c(Command::ProtectionStatus),
        c(Command::SourceSetMonitor(SetSourceMonitor {
            source: 3,
            monitor: MonitorMode::Also,
        })),
        c(Command::MonitorStatus),
        c(Command::MonitorDevices),
        c(Command::MonitorSetDevice(SetMonitorDevice {
            device: Some("play:{0.0.0.00000000}.{1234}".into()),
        })),
        c(Command::MonitorSetDevice(SetMonitorDevice { device: None })),
        c(Command::MonitorSetVolume(SetMonitorVolume { volume: 0.5 })),
        c(Command::MonitorSetSolo(SetMonitorSolo {
            sources: vec![3, 4],
        })),
        c(Command::LoudnessStatus),
        c(Command::LoudnessSetEnabled(SetLoudnessEnabled { on: true })),
        c(Command::LoudnessReset),
        c(Command::ReplayArm),
        c(Command::CalibrationRun(CalibrationRun {
            profile: "default".into(),
            canvas: None,
        })),
        c(Command::CalibrationRun(CalibrationRun {
            profile: "default".into(),
            canvas: Some(OutputTarget::Vertical),
        })),
        c(Command::CalibrationCancel),
        c(Command::CalibrationStatus),
        c(Command::ReplayDisarm),
        c(Command::ReplaySave(ReplaySave { seconds: None })),
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
        c(Command::CollectionSetGlide(SetCollectionGlide {
            duration_ms: 400,
            easing: GlideEasing::EaseInOut,
        })),
        c(Command::SourceSetGlide(SetSourceGlide {
            source: 3,
            glide: Some(Glide {
                duration_ms: 200,
                easing: GlideEasing::Linear,
            }),
        })),
        c(Command::SourceSetRegion(SetSourceRegion {
            source: 3,
            region: Some([10, 20, 30, 40]),
        })),
        c(Command::SourceGetRegion(SourceRef { source: 3 })),
        c(Command::SourceSetRecordSeparately(SetRecordSeparately {
            source: 3,
            enabled: Some(true),
            ..SetRecordSeparately::default()
        })),
        c(Command::SourceSetRecordSeparately(SetRecordSeparately {
            source: 3,
            video: Some(true),
            audio: Some(false),
            video_as: Some(rbs_api::request::VideoAs::Image),
            ..SetRecordSeparately::default()
        })),
        c(Command::MarkerAdd(rbs_api::request::MarkerAdd {
            label: Some("Good".into()),
            color: Some(rbs_api::request::MarkerColor::Red),
            canvas: Some(rbs_api::types::OutputTarget::All),
        })),
        c(Command::MarkerRemove(rbs_api::request::MarkerRemove {
            canvas: rbs_api::types::OutputCanvas::Vertical,
            index: 2,
        })),
        c(Command::ProjectExport(rbs_api::request::ProjectExport {
            recording: "take".into(),
            format: rbs_api::types::ProjectFormat::Fcpxml,
        })),
        c(Command::SourceSetChrome(SetSourceChrome {
            source: 3,
            chrome: Some(ChromeSpec {
                top: 40,
                right: 0,
                bottom: 0,
                left: 0,
                fill: ChromeFill::Fit,
            }),
        })),
        c(Command::SourceGetChrome(SourceRef { source: 3 })),
        c(Command::SourceFitChrome(SourceRef { source: 3 })),
        c(Command::SourceResetChrome(SourceRef { source: 3 })),
        c(Command::SceneAudioGet(SceneRef { scene: 2 })),
        c(Command::SceneAudioSet(SetSceneLevel {
            scene: 2,
            source: 3,
            volume: Some(0.25),
            muted: None,
        })),
        c(Command::SceneAudioOwn(SceneRef { scene: 2 })),
        c(Command::SceneAudioShared(SceneRef { scene: 2 })),
        c(Command::SceneAudioResetSource(SceneSourceRef {
            scene: 2,
            source: 3,
        })),
        c(Command::SettingsSet(SettingsPatch {
            profiles: Some(vec![profile()]),
            canvas: Some(canvas()),
            stream: Some(StreamSettingsPatch {
                reconnect: false,
                srt_latency_ms: Some(500),
                encoding: encoding(),
                auto_record: Some(true),
                output_size: None,
                hold_picture: Some(HoldPicture::Black),
                hold_scene: None,
                hold_sound: None,
            }),
            loudness: Some(LoudnessSettings {
                target: -16.0,
                true_peak_limit: -1.0,
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
        c(Command::SyncMeasureStart(SyncMeasureStart {
            source: 3,
            reference: None,
        })),
        c(Command::SyncMeasureStart(SyncMeasureStart {
            source: 3,
            reference: Some(4),
        })),
        c(Command::SyncMeasureCancel(SyncMeasureCancel {
            measurement: "1".into(),
        })),
        c(Command::SyncDevices),
        c(Command::SyncForgetDevice(SyncForgetDevice {
            key: DeviceKey {
                type_id: "org.rbs.source.audio-input".into(),
                adapter: "org.rbs.audio.test".into(),
                target: "a\"b".into(),
            },
        })),
        c(Command::SyncApplyToDeviceSources(
            SyncApplyToDeviceSources { source: 3 },
        )),
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
    names.dedup();
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
    round_trip(&LookList {
        looks: vec![LookInfo {
            id: "builtin:warm".into(),
            name: "Warm".into(),
            group: LookGroup::Color,
        }],
    });
    round_trip(&BoxPlaced { seq: 2, ok: true });
    assert_eq!(
        serde_json::to_value(SourceKind::Image).unwrap(),
        json!("image")
    );
    assert_eq!(
        serde_json::to_value(SourceKind::Color).unwrap(),
        json!("color")
    );
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
        estimate: None,
        skipped: vec![],
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
            vertical: None,
            loudness: Some(LoudnessSettings {
                target: -14.0,
                true_peak_limit: -1.0,
            }),
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
    round_trip(&Snapshot {
        seq: seq.clone(),
        data: replay_status(),
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
            tracks: vec![],
            skipped: vec![],
            paused: false,
            paused_ms: 0,
            recorded_ms: 0,
        }),
        EventPayload::CanvasChanged(canvas()),
        EventPayload::SettingsChanged(Settings {
            profiles: vec![],
            canvas: canvas(),
            stream: None,
            vertical: None,
            loudness: None,
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
            held: true,
            held_ms: 1500,
            hold_picture: None,
            hold_scene: None,
            hold_sound: None,
            hold_fallback: false,
            hold_sound_fallback: false,
            hold_fallback_reason: None,
        }),
        EventPayload::VcamChanged(VcamStatus {
            frames: None,
            ..vcam_status()
        }),
        EventPayload::ReplayChanged(replay_status()),
        EventPayload::MonitorChanged(monitor_status()),
        EventPayload::ProtectionChanged(protection_status()),
        EventPayload::ProtectionChanged(ProtectionStatus::idle()),
        EventPayload::RecordCanvasState(CanvasRecordState {
            canvas: OutputCanvas::Vertical,
            state: RecordState::Idle { last: None },
        }),
        EventPayload::StreamCanvasState(CanvasStreamState {
            canvas: OutputCanvas::Vertical,
            state: StreamState::Idle { last: None },
        }),
        EventPayload::CalibrationState(calibration_done()),
        EventPayload::CalibrationState(CalibrationState::Idle),
        EventPayload::SyncMeasureChanged(SyncMeasureChanged {
            measurement: "1".into(),
            source: 3,
            reference: 4,
            state: SyncMeasureState::Running { pairs: 2 },
        }),
        EventPayload::SyncMeasureChanged(SyncMeasureChanged {
            measurement: "1".into(),
            source: 3,
            reference: 4,
            state: SyncMeasureState::Done {
                suggested_offset_ms: -35,
                measured_offset_ms: -35,
                effective_offset_ms: 0,
                clamped: false,
                pairs: 5,
                spread_ms: 4.5,
            },
        }),
        EventPayload::SyncMeasureChanged(SyncMeasureChanged {
            measurement: "2".into(),
            source: 3,
            reference: 4,
            state: SyncMeasureState::Failed {
                reason: SyncMeasureFailure::TooFewClaps,
            },
        }),
    ] {
        round_trip(&Event {
            object: None,
            seq: 42,
            payload,
        });
    }
}

fn monitor_status() -> MonitorStatus {
    MonitorStatus {
        available: true,
        device: None,
        volume: 0.8,
        wanted: true,
        playing: true,
        latency_ms: Some(72),
        solo: vec![3],
        error: None,
    }
}

#[test]
fn monitor_values_have_their_wire_form() {
    let v = serde_json::to_value(SetSourceMonitor {
        source: 3,
        monitor: MonitorMode::Only,
    })
    .unwrap();
    assert_eq!(v, json!({"source": 3, "monitor": "only"}));
    assert!(
        serde_json::from_value::<SetSourceMonitor>(json!({"source": 3, "monitor": "solo"}))
            .is_err()
    );
    // `null` is System default.
    let d: SetMonitorDevice = serde_json::from_value(json!({"device": null})).unwrap();
    assert_eq!(d.device, None);
    // Off, no solo, no latency and no error are left out.
    let v = serde_json::to_value(MonitorStatus {
        playing: false,
        latency_ms: None,
        solo: Vec::new(),
        ..monitor_status()
    })
    .unwrap();
    assert!(v.get("latency_ms").is_none() && v.get("solo").is_none() && v.get("error").is_none());
    assert_eq!(v["device"], Value::Null);
    let row = serde_json::to_value(SourceInfo {
        monitor: MonitorMode::Off,
        ..source()
    })
    .unwrap();
    assert!(row.get("monitor").is_none());
    assert_eq!(serde_json::to_value(source()).unwrap()["monitor"], "only");
    round_trip(&MonitorDeviceList {
        devices: vec![MonitorDevice {
            device: "play:x".into(),
            name: "Headphones".into(),
            is_default: true,
        }],
    });
}

fn protection_status() -> ProtectionStatus {
    ProtectionStatus {
        guarded: true,
        level: ProtectionLevel::Warning,
        disks: vec![ProtectionDisk {
            level: ProtectionLevel::Warning,
            free_bytes: Some(3 << 30),
            seconds_left: Some(120),
            recordings: vec![OutputCanvas::Main],
        }],
        outputs: vec![ProtectionOutput {
            canvas: OutputCanvas::Main,
            output: OutputKind::Record,
            level: ProtectionLevel::Ok,
        }],
        render: ProtectionLevel::Ok,
        audio: ProtectionLevel::Ok,
        memory: ProtectionMemory {
            level: ProtectionLevel::Unavailable,
            available_bytes: None,
        },
        alerts: vec![ProtectionAlert {
            kind: ProtectionKind::Disk,
            level: ProtectionLevel::Warning,
            text: "Recording will stop in about 2 minutes: the disk is almost full.".into(),
            canvas: None,
            output: None,
        }],
    }
}

#[test]
fn protection_status_has_the_wire_shape_of_the_spec() {
    let v = serde_json::to_value(protection_status()).unwrap();
    assert_eq!(v["level"], "warning");
    assert_eq!(v["disks"][0]["recordings"][0], "main");
    assert_eq!(v["outputs"][0]["output"], "record");
    assert_eq!(v["memory"]["level"], "unavailable");
    assert!(v["memory"].get("available_bytes").is_none());
    assert!(v["alerts"][0].get("canvas").is_none());
    assert_eq!(v["alerts"][0]["kind"], "disk");
    let idle = serde_json::to_value(ProtectionStatus::idle()).unwrap();
    assert_eq!(idle["guarded"], false);
    assert_eq!(idle["level"], "ok");
}

fn replay_status() -> ReplayStatus {
    ReplayStatus {
        armed: true,
        held_secs: 192,
        length_secs: 300,
        bytes: 123_456_789,
        memory_limit: 1 << 30,
        memory_limited: false,
        saving: false,
        last_saved: Some("/clips/RBS Replay 2026-11-10 17-00-00.mkv".into()),
        last_error: None,
    }
}

#[test]
fn replay_save_takes_optional_seconds_and_refuses_unknown_fields() {
    let r: ReplaySave = serde_json::from_str("{}").unwrap();
    assert_eq!(r.seconds, None);
    let r: ReplaySave = serde_json::from_str(r#"{"seconds": 30}"#).unwrap();
    assert_eq!(r.seconds, Some(30));
    assert!(serde_json::from_str::<ReplaySave>(r#"{"secs": 30}"#).is_err());
    let v = serde_json::to_value(ReplayStatus {
        last_saved: None,
        ..replay_status()
    })
    .unwrap();
    assert!(v.get("last_saved").is_none() && v.get("last_error").is_none());
    assert_eq!(v["held_secs"], 192);
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
fn canvas_states_are_the_main_object_with_canvas_added() {
    let event = Event {
        object: Some(OutputCanvas::Vertical.object()),
        seq: 3,
        payload: EventPayload::RecordCanvasState(CanvasRecordState {
            canvas: OutputCanvas::Vertical,
            state: RecordState::Recording {
                profile: Some("default".into()),
                video_encoder: "NVENC".into(),
                audio_encoder: None,
                tracks: vec![],
                skipped: vec![],
                paused: true,
                paused_ms: 1500,
                recorded_ms: 61_250,
            },
        }),
    };
    assert_eq!(event.key().to_string(), "record.canvas_state:1");
    let v: Value = serde_json::from_str(&JsonCodec.encode_event(&event)).unwrap();
    assert_eq!(
        v["params"],
        json!({"topic": "record.canvas_state", "object": 1, "seq": 3, "data": {
            "canvas": "vertical", "state": "recording", "profile": "default",
            "video_encoder": "NVENC", "audio_encoder": null,
            "paused": true, "paused_ms": 1500, "recorded_ms": 61_250}})
    );
}

#[test]
fn hold_methods_take_a_canvas_and_a_picture() {
    let decode = |text: &str| match JsonCodec.decode(text).map(|m| m.request) {
        Ok(Request::Command(c)) => Ok(c),
        Ok(other) => panic!("not a command: {other:?}"),
        Err(e) => Err(e.error.kind),
    };
    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","id":1,"method":"stream.hold"}"#),
        Ok(Command::StreamHold(StreamHold::default()))
    );
    assert_eq!(
        StreamHold::default().picture.unwrap_or_default(),
        HoldPicture::Freeze
    );
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"stream.hold_toggle","params":{"canvas":"all","picture":"black"}}"#
        ),
        Ok(Command::StreamHoldToggle(StreamHold {
            canvas: Some(OutputTarget::All),
            picture: Some(HoldPicture::Black),
            scene: None,
            sound: None,
        }))
    );
    // Protocol 1.67: a scene, with its sound; an unknown picture is still refused.
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"stream.hold","params":{"picture":"scene","scene":7,"sound":"scene"}}"#
        ),
        Ok(Command::StreamHold(StreamHold {
            canvas: None,
            picture: Some(HoldPicture::Scene),
            scene: Some(7),
            sound: Some(HoldSound::Scene),
        }))
    );
    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","id":1,"method":"stream.hold","params":{"picture":"slate"}}"#),
        Err(ErrorKind::InvalidParams)
    );
    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","id":1,"method":"stream.hold","params":{"sound":"loud"}}"#),
        Err(ErrorKind::InvalidParams)
    );
    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","id":1,"method":"output.break_toggle"}"#),
        Ok(Command::OutputBreakToggle)
    );
    for method in [
        "stream.hold",
        "stream.resume",
        "stream.hold_toggle",
        "output.break_toggle",
    ] {
        assert_eq!(
            rbs_api::request::method_scope(method),
            Some(Some(Scope::Control)),
            "{method}"
        );
    }
    let entry = OutputEntry {
        canvas: OutputCanvas::Main,
        kind: Some(OutputKind::Stream),
        outcome: OutputOutcome::Held,
        video_encoder: None,
        audio_encoder: None,
        error: None,
        estimate: None,
        skipped: Vec::new(),
    };
    assert_eq!(
        serde_json::to_value(&entry).unwrap(),
        json!({"canvas": "main", "kind": "stream", "outcome": "held"})
    );
}

#[test]
fn output_methods_take_an_optional_canvas() {
    let decode = |text: &str| match JsonCodec.decode(text).map(|m| m.request) {
        Ok(Request::Command(c)) => Ok(c),
        Ok(other) => panic!("not a command: {other:?}"),
        Err(e) => Err(e.error.kind),
    };
    let on = |canvas| OnCanvas { canvas };
    let many = |canvas| OnCanvases { canvas };
    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","id":1,"method":"stream.stop"}"#),
        Ok(Command::StreamStop(many(None)))
    );
    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","id":1,"method":"stream.stop","params":{"canvas":null}}"#),
        Ok(Command::StreamStop(many(None)))
    );
    assert_eq!(many(None).target(), OutputTarget::Main);
    assert_eq!(on(None).canvas(), OutputCanvas::Main);
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"record.status","params":{"canvas":"vertical"}}"#
        ),
        Ok(Command::RecordStatus(on(Some(OutputCanvas::Vertical))))
    );
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"record.start","params":{"profile":"default","canvas":"vertical"}}"#
        ),
        Ok(Command::RecordStart(RecordStart {
            profile: "default".into(),
            canvas: Some(OutputTarget::Vertical),
            iso: None,
            acknowledge_heavy: None,
            on_hardware_limit: None,
        }))
    );
    // `all` (protocol 1.42) on the four start and stop methods only.
    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","id":1,"method":"stream.start","params":{"canvas":"all"}}"#),
        Ok(Command::StreamStart(many(Some(OutputTarget::All))))
    );
    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","id":1,"method":"record.status","params":{"canvas":"all"}}"#),
        Err(ErrorKind::InvalidParams)
    );
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"record.start_to_file","params":{"profile":"default","path":"/a.mkv","canvas":"all"}}"#
        ),
        Err(ErrorKind::InvalidParams)
    );
    assert_eq!(OutputTarget::All.single(), None);
    assert_eq!(
        OutputTarget::Vertical.single(),
        Some(OutputCanvas::Vertical)
    );
    // Unknown fields are still ignored, an unknown canvas is not.
    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","id":1,"method":"stream.start","params":{"later":1}}"#),
        Ok(Command::StreamStart(many(None)))
    );
    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","id":1,"method":"stream.start","params":{"canvas":"square"}}"#),
        Err(ErrorKind::InvalidParams)
    );
}

#[test]
fn an_all_answer_lists_each_canvas_main_first() {
    let refused = ApiError::new(
        ErrorKind::Failed,
        "no server is saved for the vertical stream",
    );
    let answer = Response::Outputs(OutputsResult {
        outputs: vec![
            OutputEntry {
                kind: None,
                canvas: OutputCanvas::Main,
                outcome: OutputOutcome::Started,
                video_encoder: Some("NVENC".into()),
                audio_encoder: None,
                error: None,
                estimate: None,
                skipped: vec![],
            },
            OutputEntry {
                kind: None,
                canvas: OutputCanvas::Vertical,
                outcome: OutputOutcome::Refused,
                video_encoder: None,
                audio_encoder: None,
                error: Some(OutputError::from(&refused)),
                estimate: None,
                skipped: vec![],
            },
        ],
    });
    assert_eq!(
        serde_json::to_value(&answer).unwrap(),
        json!({"outputs": [
            {"canvas": "main", "outcome": "started", "video_encoder": "NVENC"},
            {"canvas": "vertical", "outcome": "refused", "error": {
                "code": -32040,
                "message": "no server is saved for the vertical stream",
                "data": {"kind": "failed"}}}
        ]})
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
        held: false,
        held_ms: 0,
        hold_picture: None,
        hold_scene: None,
        hold_sound: None,
        hold_fallback: false,
        hold_sound_fallback: false,
        hold_fallback_reason: None,
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
            "held": false,
            "held_ms": 0,
            "hold_picture": null,
            "hold_scene": null,
            "hold_sound": null,
            "hold_fallback": false,
            "hold_sound_fallback": false,
            "hold_fallback_reason": null,
        })
    );
    // A 1.63 server's live state, without the hold fields, reads as not held.
    let old: StreamState = serde_json::from_value(json!({
        "state": "live", "protocol": "rtmp", "connected": true, "connects": 1,
        "reconnect_attempts": 0, "video_encoder": "VT", "audio_encoder": null,
    }))
    .unwrap();
    assert!(matches!(
        old,
        StreamState::Live {
            held: false,
            held_ms: 0,
            hold_picture: None,
            hold_fallback: false,
            ..
        }
    ));
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
        vertical: None,
        loudness: None,
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
            output_size: None,
            hold_picture: Some(HoldPicture::Black),
            hold_scene: Some(Some(4)),
            hold_sound: Some(HoldSound::Scene),
        }
    );
    // Protocol 1.67: `null` clears the hold scene, absent leaves it.
    let cleared: StreamSettingsPatch = serde_json::from_value({
        let mut v = serde_json::to_value(stream_settings()).unwrap();
        v["hold_scene"] = serde_json::Value::Null;
        v
    })
    .unwrap();
    assert_eq!(cleared.hold_scene, Some(None));
    // Protocol 1.65 added `hold_picture`: a 1.64 server's settings read as `freeze`,
    // and a 1.64 client's patch leaves it as it is.
    let mut older = serde_json::to_value(stream_settings()).unwrap();
    assert_eq!(older["hold_picture"], "black");
    older.as_object_mut().unwrap().remove("hold_picture");
    let read: StreamSettings = serde_json::from_value(older.clone()).unwrap();
    assert_eq!(read.hold_picture, HoldPicture::Freeze);
    let patch: StreamSettingsPatch = serde_json::from_value(older).unwrap();
    assert_eq!(patch.hold_picture, None);
    // Protocol 1.67 added `hold_scene` and `hold_sound`: a 1.66 server's settings
    // read as none and silence, and a 1.66 client's patch leaves them.
    let mut older = serde_json::to_value(stream_settings()).unwrap();
    for field in ["hold_scene", "hold_sound"] {
        older.as_object_mut().unwrap().remove(field);
    }
    let read: StreamSettings = serde_json::from_value(older.clone()).unwrap();
    assert_eq!(
        (read.hold_scene, read.hold_sound),
        (None, HoldSound::Silence)
    );
    let patch: StreamSettingsPatch = serde_json::from_value(older).unwrap();
    assert_eq!((patch.hold_scene, patch.hold_sound), (None, None));
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

/// Protocol 1.39 added `output_size` (ADR-0102). Omitted and `null` are the same as
/// the canvas, so a 1.38 client still reads the object. A size is two sides.
#[test]
fn output_size_is_omitted_when_it_matches_the_canvas() {
    let text = serde_json::to_value(profile()).unwrap();
    assert!(text.get("output_size").is_none(), "{text}");
    let stream = serde_json::to_value(stream_settings()).unwrap();
    assert!(stream.get("output_size").is_none(), "{stream}");

    let mut with_null = text.clone();
    with_null["output_size"] = Value::Null;
    let read: RecordingProfile = serde_json::from_value(with_null).unwrap();
    assert_eq!(read.output_size, None);
    let mut stream_null = stream.clone();
    stream_null["output_size"] = Value::Null;
    let patch: StreamSettingsPatch = serde_json::from_value(stream_null).unwrap();
    assert_eq!(patch.output_size, None);

    let mut sized = profile();
    sized.output_size = Some(Dimensions {
        width: 1280,
        height: 720,
    });
    let text = serde_json::to_value(&sized).unwrap();
    assert_eq!(text["output_size"]["width"], 1280);
    assert_eq!(text["output_size"]["height"], 720);
    let back: RecordingProfile = serde_json::from_value(text.clone()).unwrap();
    assert_eq!(back.output_size, sized.output_size);
    // One side only is not a size.
    let mut half = text;
    half["output_size"] = json!({"width": 1280});
    assert!(serde_json::from_value::<RecordingProfile>(half).is_err());
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
        glide: None,
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
fn looks_samples_and_boxes_decode_as_protocol_1_34() {
    let list: Request =
        serde_json::from_value(json!({"method": "look.list", "params": {}})).unwrap();
    assert_eq!(list, Request::Command(Command::LookList));
    assert_eq!(list.scope(), Some(Scope::Read));
    let apply: Request = serde_json::from_value(json!({
        "method": "look.apply",
        "params": {"source": 4, "look": "builtin:circle"}
    }))
    .unwrap();
    assert_eq!(apply.scope(), Some(Scope::Control));
    assert!(
        serde_json::from_value::<ApplyLook>(json!({"source": 1, "look": "x", "extra": 1})).is_err()
    );
    let sample: Request = serde_json::from_value(json!({
        "method": "source.add_sample",
        "params": {"sample": "aurora"}
    }))
    .unwrap();
    assert_eq!(
        sample,
        Request::Command(Command::SourceAddSample(AddSample {
            sample: "aurora".into()
        }))
    );
    // gesture and seq default to 0, each call its own undo step.
    let bare: SetItemBox = serde_json::from_value(json!({
        "source": 3,
        "position": [0.0, 0.0],
        "size": [100.0, 50.0]
    }))
    .unwrap();
    assert_eq!(bare.gesture, 0);
    assert_eq!(bare.seq, 0);
    assert_eq!(
        serde_json::to_value(bare).unwrap(),
        json!({"source": 3, "position": [0.0, 0.0], "size": [100.0, 50.0]})
    );
    let placed: BoxPlaced = serde_json::from_value(json!({"seq": 0, "ok": true})).unwrap();
    assert!(placed.ok);
    let unknown: LookInfo = serde_json::from_value(json!({
        "id": "builtin:later",
        "name": "Later",
        "group": "grade"
    }))
    .unwrap();
    assert_eq!(unknown.group, LookGroup::Other);
}

#[test]
fn glide_commands_decode_as_protocol_1_35() {
    let set: Request = serde_json::from_value(json!({
        "method": "collection.set_glide",
        "params": {"duration_ms": 250, "easing": "linear"}
    }))
    .unwrap();
    assert_eq!(
        set,
        Request::Command(Command::CollectionSetGlide(SetCollectionGlide {
            duration_ms: 250,
            easing: GlideEasing::Linear,
        }))
    );
    assert_eq!(set.scope(), Some(Scope::Control));
    let clear: Request = serde_json::from_value(json!({
        "method": "source.set_glide",
        "params": {"source": 3, "glide": null}
    }))
    .unwrap();
    assert_eq!(
        clear,
        Request::Command(Command::SourceSetGlide(SetSourceGlide {
            source: 3,
            glide: None,
        }))
    );
    let override_: Request = serde_json::from_value(json!({
        "method": "source.set_glide",
        "params": {"source": 3, "glide": {"duration_ms": 100, "easing": "ease-in-out"}}
    }))
    .unwrap();
    assert_eq!(
        override_,
        Request::Command(Command::SourceSetGlide(SetSourceGlide {
            source: 3,
            glide: Some(Glide {
                duration_ms: 100,
                easing: GlideEasing::EaseInOut,
            }),
        }))
    );
    // Default collection glide is left out of scene.list.
    let mut list = scenes();
    list.master = Master::default();
    list.glide = Glide::default();
    let json = serde_json::to_string(&list).unwrap();
    assert!(!json.contains("glide"), "{json}");
    let with = Glide {
        duration_ms: 0,
        easing: GlideEasing::Linear,
    };
    list.glide = with;
    assert_eq!(
        serde_json::to_value(&list).unwrap()["glide"],
        json!({"duration_ms": 0, "easing": "linear"})
    );
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

#[test]
fn the_unavailable_source_status_round_trips_and_older_clients_read_it_as_failed() {
    let unavailable = SourceStatus::Unavailable {
        fault: SourceFault::TargetGone,
        detail: "the window was closed".into(),
    };
    round_trip(&unavailable);
    let wire = serde_json::to_value(&unavailable).unwrap();
    assert_eq!(
        wire,
        json!({"state": "unavailable", "fault": "target_gone", "detail": "the window was closed"})
    );
    round_trip(&SourceStatus::Running);
    round_trip(&SourceStatus::Idle);
    round_trip(&SourceStatus::Failed { reason: "x".into() });
    // A state a newer server adds reads as failed, with its words.
    let newer: SourceStatus =
        serde_json::from_value(json!({"state": "paused", "reason": "held"})).unwrap();
    assert_eq!(
        newer,
        SourceStatus::Failed {
            reason: "held".into()
        }
    );
    // An unknown fault reads as other.
    let fault: SourceStatus = serde_json::from_value(
        json!({"state": "unavailable", "fault": "on_fire", "detail": "hot"}),
    )
    .unwrap();
    assert_eq!(
        fault,
        SourceStatus::Unavailable {
            fault: SourceFault::Other,
            detail: "hot".into()
        }
    );
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
fn separate_files_round_trip_and_later_options_are_refused() {
    let decode = |text: &str| match JsonCodec.decode(text).map(|m| m.request) {
        Ok(Request::Command(c)) => Ok(c),
        Ok(other) => panic!("not a command: {other:?}"),
        Err(e) => Err(e.error.kind),
    };
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"source.set_record_separately","params":{"source":4,"enabled":true}}"#
        ),
        Ok(Command::SourceSetRecordSeparately(SetRecordSeparately {
            source: 4,
            enabled: Some(true),
            ..SetRecordSeparately::default()
        }))
    );
    // 1.52: the feeds and the picture's form, each optional.
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"source.set_record_separately","params":{"source":4,"video":true,"video_as":"image"}}"#
        ),
        Ok(Command::SourceSetRecordSeparately(SetRecordSeparately {
            source: 4,
            video: Some(true),
            video_as: Some(rbs_api::request::VideoAs::Image),
            ..SetRecordSeparately::default()
        }))
    );
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"source.set_record_separately","params":{"source":4,"video_as":"gif"}}"#
        ),
        Err(ErrorKind::InvalidParams)
    );
    // Per-track options are later minors (ADR-0086 decision 2).
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"source.set_record_separately","params":{"source":4,"enabled":true,"raw":true}}"#
        ),
        Err(ErrorKind::InvalidParams)
    );
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"record.start","params":{"profile":"default","iso":false}}"#
        ),
        Ok(Command::RecordStart(RecordStart {
            profile: "default".into(),
            canvas: None,
            iso: Some(false),
            acknowledge_heavy: None,
            on_hardware_limit: None,
        }))
    );
    let running = RecordState::Recording {
        profile: Some("default".into()),
        video_encoder: "VT".into(),
        audio_encoder: Some("AAC".into()),
        tracks: vec![RecordTrack {
            file: "Microphone.wav".into(),
            role: TrackRole::Audio,
            source: 4,
        }],
        skipped: vec![SkippedTrack {
            source: 5,
            role: TrackRole::Audio,
            reason: SkipReason::NotHeard,
        }],
        paused: false,
        paused_ms: 0,
        recorded_ms: 0,
    };
    round_trip(&running);
    let value = serde_json::to_value(&running).unwrap();
    assert_eq!(
        value["tracks"],
        json!([{"file": "Microphone.wav", "role": "audio", "source": 4}])
    );
    assert_eq!(
        value["skipped"],
        json!([{"source": 5, "role": "audio", "reason": "not_heard"}])
    );
    // A later minor's role or reason reads as `other`.
    let later: SkippedTrack =
        serde_json::from_value(json!({"source": 1, "role": "caption", "reason": "disk_full"}))
            .unwrap();
    assert_eq!(
        (later.role, later.reason),
        (TrackRole::Other, SkipReason::Other)
    );
    // A recording without separate files reads as it did in 1.45.
    let plain = serde_json::to_value(RecordState::Recording {
        profile: None,
        video_encoder: "VT".into(),
        audio_encoder: None,
        tracks: vec![],
        skipped: vec![],
        paused: false,
        paused_ms: 0,
        recorded_ms: 0,
    })
    .unwrap();
    assert!(plain.get("tracks").is_none());
    let mut source = source();
    let marked = serde_json::to_value(&source).unwrap();
    assert_eq!(
        marked["record_tracks"],
        json!({"video": true, "video_as": "image"})
    );
    source.record_separately = false;
    source.record_tracks = None;
    let plain = serde_json::to_value(&source).unwrap();
    assert!(plain.get("record_separately").is_none());
    assert!(plain.get("record_tracks").is_none());
    // Roles and reasons of 1.52.
    let started = serde_json::to_value(rbs_api::types::RecordTrack {
        file: "Logo.png".into(),
        role: rbs_api::types::TrackRole::Image,
        source: 2,
    })
    .unwrap();
    assert_eq!(started["role"], "image");
    let skipped = serde_json::to_value(rbs_api::types::SkippedTrack {
        source: 2,
        role: rbs_api::types::TrackRole::Video,
        reason: rbs_api::types::SkipReason::Limit,
    })
    .unwrap();
    assert_eq!(skipped["reason"], "limit");
}

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

#[test]
fn scene_levels_are_optional_fields_and_their_answers_round_trip() {
    // Protocol 1.48 (ADR-0091): a row without an entry and a scene in Shared levels
    // look as they did before.
    let row = serde_json::to_value(SourceInfo {
        heard: None,
        ..source()
    })
    .unwrap();
    assert!(row.get("heard").is_none());
    assert_eq!(
        serde_json::to_value(source()).unwrap()["heard"],
        json!({"volume": 1.0, "muted": false})
    );
    let list = serde_json::to_value(scenes()).unwrap();
    assert_eq!(list["scenes"][0]["own_audio"], true);
    assert!(list["scenes"][1].get("own_audio").is_none());
    let older: SceneInfo = serde_json::from_value(json!({"id": 1, "name": "A"})).unwrap();
    assert!(!older.own_audio);
    round_trip(&SceneLevels {
        scene: 2,
        own: true,
        levels: vec![SceneLevelInfo {
            source: 3,
            volume: None,
            muted: Some(true),
        }],
    });
    round_trip(&SceneLevelSet { turned_own: true });
    // Only the given fields are sent.
    let set = serde_json::to_value(SetSceneLevel {
        scene: 2,
        source: 3,
        volume: None,
        muted: Some(true),
    })
    .unwrap();
    assert_eq!(set, json!({"scene": 2, "source": 3, "muted": true}));
}

#[test]
fn markers_round_trip_and_unknown_colours_are_refused() {
    use rbs_api::request::{MarkerAdd, MarkerColor, MarkerPlaced, MarkerRemove, MarkersAdded};
    use rbs_api::types::{OutputCanvas, OutputTarget};
    let decode = |text: &str| match JsonCodec.decode(text).map(|m| m.request) {
        Ok(Request::Command(c)) => Ok(c),
        Ok(other) => panic!("not a command: {other:?}"),
        Err(e) => Err(e.error.kind),
    };
    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","id":1,"method":"marker.add","params":{}}"#),
        Ok(Command::MarkerAdd(MarkerAdd::default()))
    );
    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","id":1,"method":"marker.add"}"#),
        Ok(Command::MarkerAdd(MarkerAdd::default())),
        "params may be left out"
    );
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"marker.add","params":{"label":"Good take","color":"green","canvas":"vertical"}}"#
        ),
        Ok(Command::MarkerAdd(MarkerAdd {
            label: Some("Good take".into()),
            color: Some(MarkerColor::Green),
            canvas: Some(OutputTarget::Vertical),
        }))
    );
    for bad in [
        r#"{"jsonrpc":"2.0","id":1,"method":"marker.add","params":{"color":"pink"}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"marker.add","params":{"note":"x"}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"marker.remove","params":{"canvas":"all","index":1}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"marker.remove","params":{"canvas":"main"}}"#,
    ] {
        assert_eq!(decode(bad), Err(ErrorKind::InvalidParams), "{bad}");
    }
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"marker.remove","params":{"canvas":"main","index":3}}"#
        ),
        Ok(Command::MarkerRemove(MarkerRemove {
            canvas: OutputCanvas::Main,
            index: 3,
        }))
    );
    let added = MarkersAdded {
        markers: vec![MarkerPlaced {
            canvas: OutputCanvas::Main,
            index: 1,
            t_ns: 12_500_000_000,
            while_paused: false,
        }],
    };
    round_trip(&added);
    assert_eq!(
        serde_json::to_value(&added).unwrap(),
        json!({"markers": [{"canvas": "main", "index": 1, "t_ns": 12_500_000_000u64}]})
    );
    // Protocol 1.63: a marker added while paused says so.
    let paused = MarkersAdded {
        markers: vec![MarkerPlaced {
            canvas: OutputCanvas::Main,
            index: 2,
            t_ns: 7,
            while_paused: true,
        }],
    };
    round_trip(&paused);
    assert_eq!(
        serde_json::to_value(&paused).unwrap()["markers"][0]["while_paused"],
        true
    );
    assert_eq!(
        rbs_api::request::method_scope("marker.add"),
        Some(Some(rbs_api::Scope::Control))
    );
    assert_eq!(
        rbs_api::request::method_scope("marker.remove"),
        Some(Some(rbs_api::Scope::Control))
    );
}

#[test]
fn project_export_round_trips_and_the_profile_omits_an_unset_editor_project() {
    use rbs_api::request::{ProjectExport, ProjectExported};
    use rbs_api::types::ProjectFormat;
    let decode = |text: &str| match JsonCodec.decode(text).map(|m| m.request) {
        Ok(Request::Command(c)) => Ok(c),
        Ok(other) => panic!("not a command: {other:?}"),
        Err(e) => Err(e.error.kind),
    };
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"project.export","params":{"recording":"RBS 2026-11-15 20-00-00","format":"fcpxml"}}"#
        ),
        Ok(Command::ProjectExport(ProjectExport {
            recording: "RBS 2026-11-15 20-00-00".into(),
            format: ProjectFormat::Fcpxml,
        }))
    );
    for bad in [
        r#"{"jsonrpc":"2.0","id":1,"method":"project.export","params":{"recording":"x","format":"aaf"}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"project.export","params":{"recording":"x"}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"project.export","params":{"format":"fcpxml"}}"#,
    ] {
        assert_eq!(decode(bad), Err(ErrorKind::InvalidParams), "{bad}");
    }
    let answer = ProjectExported {
        file: "take.fcpxml".into(),
        missing: vec!["Mic.wav".into()],
    };
    round_trip(&answer);
    assert_eq!(
        serde_json::to_value(&answer).unwrap(),
        json!({"file": "take.fcpxml", "missing": ["Mic.wav"]})
    );
    assert_eq!(
        rbs_api::request::method_scope("project.export"),
        Some(Some(rbs_api::Scope::Control))
    );
    let mut p = profile();
    assert!(
        serde_json::to_value(&p)
            .unwrap()
            .get("editor_project")
            .is_none()
    );
    p.editor_project = Some(ProjectFormat::Fcpxml);
    assert_eq!(
        serde_json::to_value(&p).unwrap()["editor_project"],
        json!("fcpxml")
    );
    round_trip(&p);
    p.editor_project = Some(ProjectFormat::Xmeml);
    assert_eq!(
        serde_json::to_value(&p).unwrap()["editor_project"],
        json!("xmeml")
    );
    round_trip(&p);
    // Markers (protocol 1.62): the defaults are omitted, a change is written whole.
    assert!(serde_json::to_value(&p).unwrap().get("markers").is_none());
    p.markers.lead_ms = 1_500;
    p.markers.stream_events = true;
    assert_eq!(
        serde_json::to_value(&p).unwrap()["markers"],
        json!({"lead_ms": 1500, "scene_changes": true, "stream_events": true,
            "pause_markers": true})
    );
    round_trip(&p);
    // Protocol 1.63: pause markers, on by default; a 1.62 client's object reads with
    // them on.
    p.markers.pause_markers = false;
    round_trip(&p);
    assert_eq!(
        serde_json::to_value(&p).unwrap()["markers"]["pause_markers"],
        false
    );
    let partial: rbs_api::types::RecordingMarkers =
        serde_json::from_value(json!({"scene_changes": false})).unwrap();
    assert_eq!(
        (
            partial.lead_ms,
            partial.scene_changes,
            partial.stream_events,
            partial.pause_markers
        ),
        (0, false, false, true)
    );
    assert!(
        serde_json::from_value::<rbs_api::types::RecordingMarkers>(json!({"lead": 1})).is_err()
    );
}

/// `record.estimate`'s answer (protocol 1.53): counts and bytes are written as whole
/// numbers, and names a client does not know read as `other`.
#[test]
fn the_estimate_answer_writes_whole_counts_and_reads_unknown_names_as_other() {
    use rbs_api::types::{
        EstimateAnswer, EstimateCode, EstimateLevel, EstimateReason, EstimateResource, EstimateRow,
        EstimateUnit, MaxTracks, MaxTracksByKind,
    };
    let answer = EstimateAnswer {
        level: EstimateLevel::Fits,
        gate_level: EstimateLevel::Fits,
        unknown: vec![EstimateResource::EncoderSessions],
        resources: vec![EstimateRow {
            resource: EstimateResource::EncoderSessions,
            scope: Some("org.rbs.test".into()),
            need: 4.0,
            limit: None,
            unit: EstimateUnit::Sessions,
            level: Some(EstimateLevel::Unknown),
            measured: false,
            calibrate: true,
        }],
        reasons: vec![EstimateReason {
            code: EstimateCode::SessionsUnknown,
            resource: Some(EstimateResource::EncoderSessions),
            track: None,
        }],
        max_tracks: MaxTracksByKind {
            video: MaxTracks {
                count: None,
                limit: None,
                partial: true,
                unknown: vec![EstimateResource::EncoderSessions],
                at_most_now: Some(3),
                known_count: None,
            },
            audio: MaxTracks {
                count: Some(32),
                limit: Some(EstimateResource::IsoCeiling),
                partial: false,
                unknown: vec![],
                at_most_now: None,
                known_count: None,
            },
        },
        tracks: vec![],
    };
    let v = serde_json::to_value(&answer).unwrap();
    assert_eq!(v["resources"][0]["need"], json!(4));
    assert!(v["resources"][0]["need"].is_u64() || v["resources"][0]["need"].is_i64());
    assert_eq!(v["resources"][0]["limit"], json!(null));
    assert!(v["max_tracks"]["audio"].get("at_most_now").is_none());
    round_trip(&answer);
    let later: EstimateRow = serde_json::from_value(json!({
        "resource": "gpu_memory", "need": 0.5, "limit": 1.0, "unit": "percent",
        "level": "critical", "measured": true
    }))
    .unwrap();
    assert_eq!(later.resource, EstimateResource::Other);
    assert_eq!(later.unit, EstimateUnit::Other);
    assert_eq!(later.level, Some(EstimateLevel::Other));
    // A 1.56 server sends no `calibrate`: read as false (protocol 1.57).
    assert!(!later.calibrate);
    assert_eq!(v["resources"][0]["calibrate"], json!(true));
}

#[test]
fn a_refused_start_carries_reason_estimate_and_tracks_beside_kind() {
    let decode = |text: &str| match JsonCodec.decode(text).map(|m| m.request) {
        Ok(Request::Command(c)) => Ok(c),
        Ok(other) => panic!("{other:?}"),
        Err(m) => Err(m.error.kind),
    };
    // `null` is the same as absent; `cpu` is read (the host refuses it); an unknown
    // policy is `invalid_params`.
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"record.start","params":{"profile":"p","acknowledge_heavy":null,"on_hardware_limit":null}}"#
        ),
        Ok(Command::RecordStart(RecordStart::new("p")))
    );
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"record.start","params":{"profile":"p","on_hardware_limit":"cpu"}}"#
        ),
        Ok(Command::RecordStart(RecordStart {
            on_hardware_limit: Some(HardwareLimitPolicy::Cpu),
            ..RecordStart::new("p")
        }))
    );
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"record.start","params":{"profile":"p","on_hardware_limit":"gpu"}}"#
        ),
        Err(ErrorKind::InvalidParams)
    );

    let estimate: EstimateAnswer = serde_json::from_value(json!({
        "level": "over", "gate_level": "over", "unknown": [], "resources": [],
        "reasons": [], "tracks": [],
        "max_tracks": {
            "video": {"count": 3, "limit": "encoder_sessions", "partial": false, "unknown": []},
            "audio": {"count": 32, "limit": "iso_ceiling", "partial": false, "unknown": []}
        }
    }))
    .unwrap();
    let refusal = StartRefusal {
        reason: RefusalReason::HardwareLimit,
        estimate,
        tracks: vec![EstimateTrack {
            source: 4,
            role: TrackRole::Video,
        }],
    };
    let error = refusal
        .clone()
        .into_error("4 video files, the encoder takes 3");
    assert_eq!(error.kind, ErrorKind::Failed);
    assert_eq!(StartRefusal::from_error(&error), Some(refusal));
    let wire: Value = serde_json::from_str(
        &JsonCodec.encode_response(Some(&RequestId::Number(1)), &Err(error.clone())),
    )
    .unwrap();
    assert_eq!(wire["error"]["code"], json!(-32040));
    assert_eq!(wire["error"]["data"]["kind"], json!("failed"));
    assert_eq!(wire["error"]["data"]["reason"], json!("hardware_limit"));
    assert_eq!(
        wire["error"]["data"]["tracks"],
        json!([{"source": 4, "role": "video"}])
    );
    assert_eq!(
        wire["error"]["data"]["estimate"]["max_tracks"]["video"]["count"],
        json!(3)
    );

    // A `kind` member never overrides the error's kind.
    let mut data = serde_json::Map::new();
    data.insert("kind".into(), json!("not_found"));
    data.insert("reason".into(), json!("heavy"));
    let odd = ApiError::with_data(ErrorKind::Failed, "x", data);
    assert_eq!(odd.wire_data()["kind"], json!("failed"));
    // An all-canvas entry carries the same members.
    let entry = OutputError::from(&error);
    let entry = serde_json::to_value(&entry).unwrap();
    assert_eq!(entry["data"]["kind"], json!("failed"));
    assert_eq!(entry["data"]["reason"], json!("hardware_limit"));
    round_trip(&OutputError::from(&error));
    // An unknown reason reads as `other`.
    assert_eq!(
        serde_json::from_value::<RefusalReason>(json!("too_hot")).unwrap(),
        RefusalReason::Other
    );
    // Errors without data stay as before.
    assert_eq!(
        StartRefusal::from_error(&ApiError::new(ErrorKind::Failed, "x")),
        None
    );
}

fn calibration_class() -> CalibrationClass {
    CalibrationClass {
        codec: "h264".into(),
        size: "1080".into(),
        fps: "60".into(),
        rate: "cbr".into(),
    }
}

fn calibration_done() -> CalibrationState {
    CalibrationState::Done {
        partial: true,
        cancelled: false,
        saved: true,
        encoders: vec![CalibrationEncoder {
            adapter: "org.rbs.test".into(),
            class: calibration_class(),
            sessions: 4,
            pixels_per_second: 4 * 1920 * 1080 * 60,
            stopped_by: CalibrationStop::Late,
        }],
        write_speed: Some(1_000_000_000),
        write_partial: false,
        skipped: vec![
            CalibrationSkip {
                code: CalibrationCode::TimeLimit,
                adapter: Some("org.rbs.other".into()),
                class: Some(calibration_class()),
            },
            CalibrationSkip {
                code: CalibrationCode::NoRoom,
                adapter: None,
                class: None,
            },
        ],
        exports: vec![CalibrationExport {
            size: "1080".into(),
            format: CalibrationExportFormat::Nv12,
            time_us: 410,
            gpu_us: Some(290),
            kept_up: true,
        }],
        exports_skipped: vec![CalibrationExportSkip {
            size: "2160".into(),
            format: CalibrationExportFormat::P010,
            code: CalibrationExportCode::TimeLimit,
        }],
    }
}

/// `calibration.*` (protocol 1.55): the state's shape, its round trip, and names a
/// client does not know read as `other`.
#[test]
fn the_calibration_state_is_flat_and_reads_unknown_names_as_other() {
    for state in [
        CalibrationState::Idle,
        CalibrationState::Running {
            step: 3,
            steps: 21,
            part: CalibrationPart::Encoder,
            adapter: Some("org.rbs.test".into()),
            cancelling: false,
        },
        CalibrationState::Running {
            step: 21,
            steps: 21,
            part: CalibrationPart::Write,
            adapter: None,
            cancelling: true,
        },
        calibration_done(),
        CalibrationState::Failed {
            code: CalibrationCode::DeviceLost,
            message: "The device was lost".into(),
        },
    ] {
        round_trip(&state);
        round_trip(&Snapshot {
            seq: BTreeMap::from([("calibration.state".to_string(), 7)]),
            data: state,
        });
    }
    let v = serde_json::to_value(calibration_done()).unwrap();
    assert_eq!(v["state"], "done");
    // The class is flattened into the encoder entry.
    assert_eq!(v["encoders"][0]["codec"], "h264");
    assert_eq!(v["encoders"][0]["stopped_by"], "late");
    assert_eq!(v["skipped"][0]["code"], "time_limit");
    assert!(v["skipped"][1].get("adapter").is_none());
    assert_eq!(
        serde_json::to_value(CalibrationCode::OpenFailed).unwrap(),
        "open_failed"
    );

    let later: CalibrationState = serde_json::from_value(json!({"state": "paused"})).unwrap();
    assert_eq!(later, CalibrationState::Other);
    let code: CalibrationCode = serde_json::from_value(json!("thermal")).unwrap();
    assert_eq!(code, CalibrationCode::Other);
    let stop: CalibrationStop = serde_json::from_value(json!("thermal")).unwrap();
    assert_eq!(stop, CalibrationStop::Other);
    let part: CalibrationPart = serde_json::from_value(json!("thermal")).unwrap();
    assert_eq!(part, CalibrationPart::Other);
    // Protocol 1.57: the export part, its results and skips in their own lists, so a
    // 1.55 client never reads an export skip as the write.
    let part: CalibrationPart = serde_json::from_value(json!("export")).unwrap();
    assert_eq!(part, CalibrationPart::Export);
    assert_eq!(v["exports"][0]["format"], "nv12");
    assert_eq!(v["exports"][0]["time_us"], 410);
    assert_eq!(v["exports_skipped"][0]["code"], "time_limit");
    assert_eq!(v["skipped"].as_array().unwrap().len(), 2);
    let code: CalibrationExportCode = serde_json::from_value(json!("thermal")).unwrap();
    assert_eq!(code, CalibrationExportCode::Other);
    let format: CalibrationExportFormat = serde_json::from_value(json!("rgb")).unwrap();
    assert_eq!(format, CalibrationExportFormat::Other);
    // A 1.56 server's `done` has neither list.
    let older: CalibrationState = serde_json::from_value(json!({
        "state": "done", "partial": false, "cancelled": false, "saved": true,
        "encoders": [], "write_speed": null, "write_partial": false, "skipped": []
    }))
    .unwrap();
    assert!(matches!(older, CalibrationState::Done { ref exports, .. } if exports.is_empty()));

    // `canvas` is optional and left out when absent.
    let run = serde_json::to_value(CalibrationRun {
        profile: "default".into(),
        canvas: None,
    })
    .unwrap();
    assert_eq!(run, json!({"profile": "default"}));
}

/// Protocol 1.61 (ADR-0124): a paused recording keeps `state: "recording"` with its
/// pause fields; a 1.60 server's state reads as not paused, and the `canvas: "all"`
/// answer has its own outcomes.
#[test]
fn a_paused_recording_reads_as_recording_and_older_states_as_not_paused() {
    let paused = RecordState::Recording {
        profile: None,
        video_encoder: "VT".into(),
        audio_encoder: None,
        tracks: vec![],
        skipped: vec![],
        paused: true,
        paused_ms: 2_000,
        recorded_ms: 10_000,
    };
    round_trip(&paused);
    let value = serde_json::to_value(&paused).unwrap();
    assert_eq!(value["state"], "recording");
    assert_eq!(value["paused"], true);
    let older: RecordState = serde_json::from_value(json!({"state": "recording", "profile": null,
            "video_encoder": "VT", "audio_encoder": null}))
    .unwrap();
    assert!(matches!(
        older,
        RecordState::Recording {
            paused: false,
            paused_ms: 0,
            recorded_ms: 0,
            ..
        }
    ));
    assert_eq!(
        serde_json::to_value([
            rbs_api::types::OutputOutcome::Paused,
            rbs_api::types::OutputOutcome::Resumed
        ])
        .unwrap(),
        json!(["paused", "resumed"])
    );
}

#[test]
fn the_sync_measure_topic_is_flat_and_reads_unknown_values_as_other() {
    let changed = SyncMeasureChanged {
        measurement: "7".into(),
        source: 3,
        reference: 4,
        state: SyncMeasureState::Failed {
            reason: SyncMeasureFailure::StudioRestarted,
        },
    };
    assert_eq!(
        serde_json::to_value(&changed).unwrap(),
        json!({"measurement": "7", "source": 3, "reference": 4, "state": "failed", "reason": "studio_restarted"})
    );
    let newer: SyncMeasureChanged = serde_json::from_value(
        json!({"measurement": "7", "source": 3, "reference": 4, "state": "paused"}),
    )
    .unwrap();
    assert_eq!(newer.state, SyncMeasureState::Other);
    let reason: SyncMeasureFailure = serde_json::from_value(json!("too_loud")).unwrap();
    assert_eq!(reason, SyncMeasureFailure::Other);
    let entry: DeviceSyncEntry = serde_json::from_value(json!({
        "key": {"type": "org.rbs.source.audio-input", "adapter": "a", "target": "t"},
        "offset_ms": 40, "origin": "estimated", "set_at": 1
    }))
    .unwrap();
    assert_eq!(entry.origin, DeviceSyncOrigin::Other);
}

#[test]
fn text_sources_decode_as_protocol_1_71() {
    let add: AddSource = serde_json::from_value(json!({"kind": "text", "text": "Live"})).unwrap();
    assert_eq!(add.kind, SourceKind::Text);
    assert_eq!(add.text.as_deref(), Some("Live"));
    assert!(add.adapter.is_empty() && add.target.is_empty());
    assert!(
        serde_json::from_value::<SetText>(json!({"source": 1, "text": "x", "file": "y"})).is_err()
    );
    round_trip(&TextFonts {
        families: vec!["Inter".into(), "Noto Sans".into()],
    });
    assert_eq!(ErrorKind::Forbidden.name(), "forbidden");
    assert_eq!(ErrorKind::NotReady.name(), "not_ready");
    assert_eq!(
        serde_json::to_value(ErrorKind::NotReady).unwrap(),
        json!("not_ready")
    );
}
