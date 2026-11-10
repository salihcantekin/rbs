//! Contract tests for the source type registry (spec `docs/specs/scene-sources.md`).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::Arc;

use rbs_audio::AudioGraphConfig;
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::Size;
use rbs_core::time::TestClock;
use rbs_core::timing::FrameRate;
use rbs_gpu::node::GpuRuntime;
use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::Node;
use rbs_scene::SceneCollection;
use rbs_scene::binder::{NodeTypes, Role, SceneBinder};
use rbs_scene::model::{Crop, ItemContent, Layout, SceneScope, SettingValue, Settings, Source};
use rbs_sources::types::{self, keys};
use rbs_sources::{
    InstanceContext, SourceError, SourceInstance, SourceTypes, VideoSource, node_layers,
};

const AUDIO: PartitionId = PartitionId(3);

fn registry() -> (SourceTypes, Arc<TestAudioBackend>) {
    let mut reg = SourceTypes::new();
    reg.register_capture(Arc::new(TestCaptureFactory::new()))
        .unwrap();
    let audio = Arc::new(TestAudioBackend::new());
    reg.register_audio(audio.clone()).unwrap();
    reg.probe();
    (reg, audio)
}

fn ctx() -> InstanceContext {
    InstanceContext::new(Arc::new(TestClock::new()), AudioGraphConfig::default())
}

fn ty(s: &str) -> NodeTypeId {
    NodeTypeId::new(s)
}

/// A source as the collection holds it, built through a collection so it is valid.
fn source(type_id: &str, settings: Settings) -> Source {
    let mut col = SceneCollection::new();
    let (id, _) = col.add_source("S", ty(type_id), settings).unwrap();
    col.source(id).unwrap().clone()
}

fn settings(pairs: &[(&str, SettingValue)]) -> Settings {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn text(s: &str) -> SettingValue {
    SettingValue::Text(s.to_owned())
}

fn video(i: SourceInstance) -> VideoSource {
    match i {
        SourceInstance::Video(v) => v,
        other => panic!("expected video, got {other:?}"),
    }
}

#[test]
fn types_are_installed_only_by_probed_adapters_that_capture_their_kind() {
    let mut reg = SourceTypes::new();
    // Still sources need no adapter (ADR-0074).
    let still = [types::IMAGE_SOURCE, types::COLOR_SOURCE].map(ty);
    assert_eq!(reg.installed(), still);
    reg.register_capture(Arc::new(TestCaptureFactory::new()))
        .unwrap();
    reg.register_audio(Arc::new(TestAudioBackend::new()))
        .unwrap();
    assert_eq!(reg.installed(), still, "no adapter type before probing");
    reg.probe();
    // The test audio backend captures inputs and loopback, not applications.
    assert_eq!(
        reg.installed(),
        [
            types::DISPLAY_SOURCE,
            types::WINDOW_SOURCE,
            types::CAMERA_SOURCE,
            types::AUDIO_INPUT_SOURCE,
            types::AUDIO_OUTPUT_SOURCE,
            types::IMAGE_SOURCE,
            types::COLOR_SOURCE,
        ]
        .map(ty)
    );
    let table = reg.node_types();
    assert!(
        table
            .descriptor(&ty(types::APPLICATION_AUDIO_SOURCE))
            .is_none()
    );
    assert!(table.descriptor(&ty("org.rbs.compositor")).is_some());
    assert!(table.descriptor(&ty("org.rbs.audio.mixer")).is_some());
    // A second adapter with the same id is refused.
    assert!(
        reg.register_capture(Arc::new(TestCaptureFactory::new()))
            .is_err()
    );
}

#[test]
fn the_binder_type_ids_match_the_node_crates() {
    assert_eq!(
        rbs_scene::binder::COMPOSITOR_TYPE,
        rbs_gpu::node::COMPOSITOR_NODE_TYPE
    );
    assert_eq!(rbs_scene::binder::MIXER_TYPE, rbs_audio::MIXER_NODE_TYPE);
}

/// Lists nothing but ranks first (hardware), so it is the best adapter.
struct Unlisted(TestCaptureFactory);

impl rbs_core::registry::AdapterFactory for Unlisted {
    fn id(&self) -> rbs_core::registry::AdapterId {
        rbs_core::registry::AdapterId::new("org.example.unlisted")
    }
    fn display_name(&self) -> &str {
        "Unlisted"
    }
    fn probe(&self) -> Result<rbs_core::capability::CapabilitySet, rbs_core::registry::ProbeError> {
        let mut caps = self.0.probe()?;
        caps.insert(
            rbs_core::registry::HARDWARE,
            rbs_core::capability::CapabilityValue::Flag(true),
        );
        Ok(caps)
    }
}

impl rbs_capture::CaptureFactory for Unlisted {
    fn enumerate(
        &self,
        _: rbs_capture::TargetKind,
    ) -> Result<Vec<rbs_capture::CaptureTarget>, rbs_capture::CaptureError> {
        Ok(Vec::new())
    }
    fn open(
        &self,
        request: &rbs_capture::CaptureRequest,
    ) -> Result<Box<dyn rbs_capture::CaptureSession>, rbs_capture::CaptureError> {
        self.0.open(request)
    }
}

#[test]
fn a_target_goes_to_the_adapter_that_lists_it() {
    let mut reg = SourceTypes::new();
    reg.register_capture(Arc::new(Unlisted(TestCaptureFactory::new())))
        .unwrap();
    reg.register_capture(Arc::new(TestCaptureFactory::new()))
        .unwrap();
    reg.probe();
    for settings in [
        Settings::new(),
        settings(&[(keys::TARGET, text("test-camera-0"))]),
    ] {
        let cam = video(
            reg.instantiate(&source(types::CAMERA_SOURCE, settings), &ctx())
                .unwrap(),
        );
        assert_eq!(cam.adapter.as_str(), rbs_capture_test::ADAPTER_ID);
    }
    // A target nobody lists (a picker-only id) goes to the best adapter.
    let picked = video(
        reg.instantiate(
            &source(
                types::CAMERA_SOURCE,
                settings(&[(keys::TARGET, text("restore-token"))]),
            ),
            &ctx(),
        )
        .unwrap(),
    );
    assert_eq!(picked.adapter.as_str(), "org.example.unlisted");
}

#[test]
fn an_empty_registry_still_has_the_compositor_the_mixer_and_the_filters() {
    let table = SourceTypes::new().node_types();
    let ids: Vec<&str> = table.ids().map(NodeTypeId::as_str).collect();
    assert_eq!(
        ids,
        [
            "org.rbs.audio-filter.gain",
            "org.rbs.audio-filter.noise-gate",
            "org.rbs.audio.mixer",
            "org.rbs.compositor",
            "org.rbs.filter.blur",
            "org.rbs.filter.chroma-key",
            "org.rbs.filter.color-correction",
            "org.rbs.filter.sharpen",
            "org.rbs.source.color",
            "org.rbs.source.image",
        ]
    );
}

#[test]
fn instances_match_the_descriptors_the_binder_uses() {
    let (reg, _) = registry();
    let table = reg.node_types();
    for t in reg.installed() {
        let settings = match t.as_str() {
            types::WINDOW_SOURCE => settings(&[(keys::TARGET, text("test-window-0"))]),
            types::IMAGE_SOURCE => settings(&[(keys::SAMPLE, text("slate"))]),
            _ => Settings::new(),
        };
        let instance = reg
            .instantiate(&source(t.as_str(), settings), &ctx())
            .unwrap();
        let descriptor: &NodeDescriptor = match &instance {
            SourceInstance::Video(v) => Node::<GpuRuntime>::descriptor(&v.node),
            SourceInstance::Audio(a) => Node::<()>::descriptor(&a.node),
            SourceInstance::Still(s) => Node::<GpuRuntime>::descriptor(&s.node),
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(
            Some(descriptor),
            table.descriptor(&t).as_deref(),
            "{t}: the controller refuses an instance whose descriptor differs"
        );
    }
}

#[test]
fn defaults_pick_the_first_display_and_camera_and_the_default_endpoint() {
    let (reg, audio) = registry();
    let display = video(
        reg.instantiate(&source(types::DISPLAY_SOURCE, Settings::new()), &ctx())
            .unwrap(),
    );
    assert_eq!(display.target.as_str(), "test-display-0");
    assert_eq!(display.adapter.as_str(), rbs_capture_test::ADAPTER_ID);
    let camera = video(
        reg.instantiate(&source(types::CAMERA_SOURCE, Settings::new()), &ctx())
            .unwrap(),
    );
    assert_eq!(camera.target.as_str(), "test-camera-0");

    let SourceInstance::Audio(mic) = reg
        .instantiate(&source(types::AUDIO_INPUT_SOURCE, Settings::new()), &ctx())
        .unwrap()
    else {
        panic!("expected audio");
    };
    assert_eq!(mic.endpoint.as_str(), rbs_audio_io_test::STEREO_INPUT);
    assert_eq!(mic.adapter.as_str(), rbs_audio_io_test::ADAPTER_ID);
    let SourceInstance::Audio(sys) = reg
        .instantiate(&source(types::AUDIO_OUTPUT_SOURCE, Settings::new()), &ctx())
        .unwrap()
    else {
        panic!("expected audio");
    };
    assert_eq!(sys.endpoint.as_str(), rbs_audio_io_test::LOOPBACK);
    // Instances hold no running stream until `prepare` (P13).
    assert_eq!(audio.active_streams(), 0);
}

#[test]
fn windows_need_a_chosen_target() {
    let (reg, _) = registry();
    assert_eq!(
        reg.instantiate(&source(types::WINDOW_SOURCE, Settings::new()), &ctx())
            .unwrap_err(),
        SourceError::NoTarget(ty(types::WINDOW_SOURCE))
    );
    let window = video(
        reg.instantiate(
            &source(
                types::WINDOW_SOURCE,
                settings(&[(keys::TARGET, text("test-window-0"))]),
            ),
            &ctx(),
        )
        .unwrap(),
    );
    assert_eq!(window.target.as_str(), "test-window-0");
}

#[test]
fn stored_targets_and_endpoints_are_used_as_given() {
    let (reg, _) = registry();
    let SourceInstance::Audio(mic) = reg
        .instantiate(
            &source(
                types::AUDIO_INPUT_SOURCE,
                settings(&[
                    (keys::ADAPTER, text(rbs_audio_io_test::ADAPTER_ID)),
                    (keys::TARGET, text(rbs_audio_io_test::MONO_INPUT)),
                ]),
            ),
            &ctx(),
        )
        .unwrap()
    else {
        panic!("expected audio");
    };
    assert_eq!(mic.endpoint.as_str(), rbs_audio_io_test::MONO_INPUT);
    // A capture target is checked when the node opens its session in `prepare`.
    let gone = video(
        reg.instantiate(
            &source(
                types::CAMERA_SOURCE,
                settings(&[(keys::TARGET, text("unplugged"))]),
            ),
            &ctx(),
        )
        .unwrap(),
    );
    assert_eq!(gone.target.as_str(), "unplugged");
    // An audio endpoint is opened now.
    assert!(matches!(
        reg.instantiate(
            &source(
                types::AUDIO_INPUT_SOURCE,
                settings(&[(keys::TARGET, text("unplugged"))]),
            ),
            &ctx(),
        ),
        Err(SourceError::Audio(_))
    ));
}

#[test]
fn bad_settings_adapters_and_types_are_refused() {
    let (reg, _) = registry();
    let refused = |type_id: &str, pairs: &[(&str, SettingValue)]| {
        reg.instantiate(&source(type_id, settings(pairs)), &ctx())
            .unwrap_err()
    };
    assert!(matches!(
        refused(
            types::CAMERA_SOURCE,
            &[(keys::WIDTH, SettingValue::Int(1280))]
        ),
        SourceError::InvalidSetting { key: "width", .. }
    ));
    assert!(matches!(
        refused(types::CAMERA_SOURCE, &[(keys::FPS, SettingValue::Int(0))]),
        SourceError::InvalidSetting { key: "fps", .. }
    ));
    assert!(matches!(
        refused(
            types::DISPLAY_SOURCE,
            &[(keys::TARGET, SettingValue::Int(1))]
        ),
        SourceError::InvalidSetting { key: "target", .. }
    ));
    assert!(matches!(
        refused(types::DISPLAY_SOURCE, &[(keys::CURSOR, text("yes"))]),
        SourceError::InvalidSetting { key: "cursor", .. }
    ));
    assert_eq!(
        refused(
            types::DISPLAY_SOURCE,
            &[(keys::ADAPTER, text("org.example.none"))]
        ),
        SourceError::AdapterUnavailable(rbs_core::registry::AdapterId::new("org.example.none"))
    );
    assert_eq!(
        refused(types::APPLICATION_AUDIO_SOURCE, &[]),
        SourceError::NoAdapter(ty(types::APPLICATION_AUDIO_SOURCE))
    );
    assert_eq!(
        refused("org.example.plugin", &[]),
        SourceError::UnknownType(ty("org.example.plugin"))
    );
    // Unknown keys are ignored, so newer documents still load.
    let ok = reg.instantiate(
        &source(
            types::CAMERA_SOURCE,
            settings(&[
                (keys::WIDTH, SettingValue::Int(1280)),
                (keys::HEIGHT, SettingValue::Int(720)),
                (keys::FPS, SettingValue::Int(30)),
                ("org.example.future", SettingValue::Bool(true)),
            ]),
        ),
        &ctx(),
    );
    assert!(ok.is_ok());
}

#[test]
fn a_capture_adapter_is_reachable_for_thumbnails() {
    let (reg, _) = registry();
    let display = &reg.targets(&ty(types::DISPLAY_SOURCE)).unwrap()[0];
    let factory = reg
        .capture_factory(&ty(types::DISPLAY_SOURCE), &display.adapter)
        .expect("the display adapter is a candidate");
    let thumb = factory
        .thumbnail(
            &rbs_capture::TargetId::new(display.target.clone()),
            Size {
                width: 96,
                height: 54,
            },
        )
        .expect("the reference adapter makes thumbnails");
    assert!(thumb.width() <= 96 && thumb.height() <= 54);
    // Audio types have no capture adapter.
    assert!(
        reg.capture_factory(&ty(types::AUDIO_INPUT_SOURCE), &display.adapter)
            .is_none()
    );
}

#[test]
fn targets_list_every_capturable_thing_for_pickers() {
    let (reg, _) = registry();
    let displays = reg.targets(&ty(types::DISPLAY_SOURCE)).unwrap();
    assert_eq!(displays.len(), 1);
    assert_eq!(displays[0].target, "test-display-0");
    assert_eq!(displays[0].adapter.as_str(), rbs_capture_test::ADAPTER_ID);
    let inputs = reg.targets(&ty(types::AUDIO_INPUT_SOURCE)).unwrap();
    let defaults: Vec<&str> = inputs
        .iter()
        .filter(|t| t.is_default)
        .map(|t| t.target.as_str())
        .collect();
    assert_eq!(defaults, [rbs_audio_io_test::STEREO_INPUT]);
    assert!(inputs.len() >= 2);
    assert!(
        reg.targets(&ty(types::APPLICATION_AUDIO_SOURCE))
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        reg.targets(&ty("org.example.plugin")),
        Err(SourceError::UnknownType(_))
    ));
}

/// Lists no displays but picks them with an OS picker, like the PipeWire portal: the
/// first pick returns a token id, later ones are cancelled.
struct Picking {
    inner: TestCaptureFactory,
    picks: std::sync::atomic::AtomicU32,
}

impl rbs_core::registry::AdapterFactory for Picking {
    fn id(&self) -> rbs_core::registry::AdapterId {
        rbs_core::registry::AdapterId::new("org.example.picking")
    }
    fn display_name(&self) -> &str {
        "Picking"
    }
    fn probe(&self) -> Result<rbs_core::capability::CapabilitySet, rbs_core::registry::ProbeError> {
        let mut caps = self.inner.probe()?;
        caps.insert(
            rbs_capture::caps::TARGET_PICKER,
            rbs_core::capability::CapabilityValue::Set(vec!["display".to_owned()]),
        );
        Ok(caps)
    }
}

impl rbs_capture::CaptureFactory for Picking {
    fn enumerate(
        &self,
        kind: rbs_capture::TargetKind,
    ) -> Result<Vec<rbs_capture::CaptureTarget>, rbs_capture::CaptureError> {
        match kind {
            rbs_capture::TargetKind::Display => {
                Err(rbs_capture::CaptureError::Unsupported("picker only"))
            }
            k => self.inner.enumerate(k),
        }
    }
    fn open(
        &self,
        request: &rbs_capture::CaptureRequest,
    ) -> Result<Box<dyn rbs_capture::CaptureSession>, rbs_capture::CaptureError> {
        self.inner.open(request)
    }
    fn pick(
        &self,
        kind: rbs_capture::TargetKind,
    ) -> Result<rbs_capture::CaptureTarget, rbs_capture::CaptureError> {
        assert_eq!(kind, rbs_capture::TargetKind::Display);
        if self
            .picks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            > 0
        {
            return Err(rbs_capture::CaptureError::Cancelled);
        }
        Ok(rbs_capture::CaptureTarget::new(
            rbs_capture::TargetId::new("portal:display:token-1"),
            rbs_capture::TargetKind::Display,
            "Picked display",
        )
        .with_size(2560, 1440))
    }
}

#[test]
fn pickers_choose_targets_that_no_adapter_lists() {
    let (plain, _) = registry();
    for t in [
        types::DISPLAY_SOURCE,
        types::CAMERA_SOURCE,
        types::AUDIO_INPUT_SOURCE,
    ] {
        assert!(plain.pickers(&ty(t)).unwrap().is_empty(), "{t}");
    }
    assert!(matches!(
        plain.pickers(&ty("org.example.plugin")),
        Err(SourceError::UnknownType(_))
    ));

    let mut reg = SourceTypes::new();
    reg.register_capture(Arc::new(Picking {
        inner: TestCaptureFactory::new(),
        picks: Default::default(),
    }))
    .unwrap();
    reg.probe();
    assert!(reg.is_installed(&ty(types::DISPLAY_SOURCE)));
    assert!(reg.targets(&ty(types::DISPLAY_SOURCE)).unwrap().is_empty());
    assert!(reg.pickers(&ty(types::CAMERA_SOURCE)).unwrap().is_empty());
    let pickers = reg.pickers(&ty(types::DISPLAY_SOURCE)).unwrap();
    assert_eq!(pickers.len(), 1);
    assert_eq!(pickers[0].adapter().as_str(), "org.example.picking");
    assert_eq!(pickers[0].adapter_name(), "Picking");

    // A picker moves to another thread, as hosts do while the dialog is up.
    let picker = pickers[0].clone();
    let picked = std::thread::spawn(move || picker.pick())
        .join()
        .unwrap()
        .unwrap();
    assert_eq!(picked.adapter.as_str(), "org.example.picking");
    assert_eq!(picked.target, "portal:display:token-1");
    assert_eq!(picked.name, "Picked display");
    assert_eq!(picked.size, Some((2560, 1440)));
    assert!(!picked.is_default);
    assert_eq!(
        pickers[0].pick(),
        Err(SourceError::Capture(rbs_capture::CaptureError::Cancelled))
    );

    // The picked target is stored like a listed one and goes to the picking adapter.
    let display = video(
        reg.instantiate(
            &source(
                types::DISPLAY_SOURCE,
                settings(&[
                    (keys::ADAPTER, text(picked.adapter.as_str())),
                    (keys::TARGET, text(&picked.target)),
                ]),
            ),
            &ctx(),
        )
        .unwrap(),
    );
    assert_eq!(display.adapter.as_str(), "org.example.picking");
    assert_eq!(display.target.as_str(), "portal:display:token-1");
}

fn layout(x: f32, w: f32) -> Layout {
    Layout::new([x, 0.0], [w, 100.0])
}

/// A scene with a display, a camera and a microphone: the binder places each node in
/// the partition its instance runs on, and the compositor gets the layer list.
#[test]
fn a_bound_scene_instantiates_into_the_right_partitions() {
    let (reg, _) = registry();
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_60)
        .unwrap();
    let (screen, _) = col
        .add_source("Screen", ty(types::DISPLAY_SOURCE), Settings::new())
        .unwrap();
    let (cam, _) = col
        .add_source("Camera", ty(types::CAMERA_SOURCE), Settings::new())
        .unwrap();
    let (mic, _) = col
        .add_source("Mic", ty(types::AUDIO_INPUT_SOURCE), Settings::new())
        .unwrap();
    let (scene, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    col.add_item(
        scene,
        ItemContent::Source(screen),
        layout(0.0, 1920.0),
        None,
    )
    .unwrap();
    let (cam_item, _) = col
        .add_item(scene, ItemContent::Source(cam), layout(1400.0, 480.0), None)
        .unwrap();
    col.add_item(scene, ItemContent::Source(mic), layout(0.0, 10.0), None)
        .unwrap();
    col.set_item_layout(
        cam_item,
        None,
        layout(1400.0, 480.0)
            .with_opacity(0.5)
            .with_crop(Crop::new(8, 0, 8, 0)),
    )
    .unwrap();
    col.set_program(canvas, Some(scene)).unwrap();

    let mut graph = GraphDescription::new();
    let mut binder = SceneBinder::new(reg.node_types()).with_audio(AUDIO);
    let report = binder.sync(&col, &mut graph).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(report.created_sources.len(), 3);
    let mut audio_nodes: Vec<NodeId> = Vec::new();
    for (id, node) in &report.created_sources {
        let instance = reg.instantiate(col.source(*id).unwrap(), &ctx()).unwrap();
        let partition = graph.node(*node).unwrap().partition;
        match instance {
            SourceInstance::Video(_) => assert_eq!(partition, PartitionId::RENDER),
            SourceInstance::Audio(_) => {
                assert_eq!(partition, AUDIO);
                audio_nodes.push(*node);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(audio_nodes, [binder.source_node(mic).unwrap()]);
    assert_eq!(report.mixers[&canvas].inputs[0].producer, audio_nodes[0]);

    // Outputs on the canvas and the mix make the plan schedule everything.
    let comp = binder.compositor(canvas, Role::Program).unwrap();
    attach(&mut graph, comp, "canvas", LinkKind::Video);
    attach(
        &mut graph,
        binder.mixer(canvas).unwrap(),
        "main",
        LinkKind::Audio,
    );
    let plan = graph.plan();
    let bound = &report.compositors[&(canvas, Role::Program)];
    let layers = node_layers(bound, &plan);
    assert_eq!(layers.len(), 2, "the microphone is not a layer");
    let producers: Vec<NodeId> = layers.iter().map(|l| l.producer).collect();
    assert_eq!(
        producers,
        [
            binder.source_node(screen).unwrap(),
            binder.source_node(cam).unwrap()
        ]
    );
    let cam_layer = layers[1];
    let expected = bound.layers[1].transform;
    assert_eq!(
        (cam_layer.transform.a, cam_layer.transform.tx),
        (expected.a, expected.tx)
    );
    assert_eq!(cam_layer.opacity, 0.5);
    assert_eq!((cam_layer.crop.left, cam_layer.crop.right), (8, 8));
    // Link hints index the compositor's scheduled links in plan order.
    for layer in &layers {
        let inputs: Vec<NodeId> = plan
            .links
            .iter()
            .filter(|l| l.to.node == comp)
            .map(|l| l.from.node)
            .collect();
        assert_eq!(inputs[layer.link], layer.producer);
    }
}

fn attach(graph: &mut GraphDescription, from: NodeId, pin: &str, kind: LinkKind) {
    let out = graph
        .add_node(Arc::new(NodeDescriptor::new(
            ty("org.example.sink"),
            NodeKind::Output,
            vec![Pin::one("in", kind)],
            vec![],
        )))
        .unwrap();
    graph.set_state(out, Lifecycle::Standby).unwrap();
    graph.set_state(out, Lifecycle::Active).unwrap();
    graph.connect(from, pin, out, "in").unwrap();
}

#[test]
fn still_types_need_no_adapter_and_make_a_still_node() {
    let reg = SourceTypes::new();
    for t in [types::IMAGE_SOURCE, types::COLOR_SOURCE] {
        assert!(reg.is_installed(&ty(t)), "{t}");
        assert!(reg.installed().contains(&ty(t)), "{t}");
        assert!(reg.node_types().descriptor(&ty(t)).is_some(), "{t}");
        assert!(reg.targets(&ty(t)).unwrap().is_empty());
    }
    let table = reg.node_types();
    let desc = table.descriptor(&ty(types::COLOR_SOURCE)).unwrap();

    let sample = source(
        types::IMAGE_SOURCE,
        settings(&[(keys::SAMPLE, text("slate"))]),
    );
    let SourceInstance::Still(st) = reg.instantiate(&sample, &ctx()).unwrap() else {
        panic!("expected a still");
    };
    assert_eq!(st.stats.picture_size(), Some((1920, 1080)));
    assert_eq!(Node::<GpuRuntime>::descriptor(&st.node), &*desc);

    let colour = source(
        types::COLOR_SOURCE,
        settings(&[
            (keys::COLOR, text("#ff0000")),
            (keys::COLOR2, text("#0000ff80")),
            (keys::ANGLE, SettingValue::Int(90)),
            (keys::WIDTH, SettingValue::Int(64)),
            (keys::HEIGHT, SettingValue::Int(36)),
        ]),
    );
    let SourceInstance::Still(st) = reg.instantiate(&colour, &ctx()).unwrap() else {
        panic!("expected a still");
    };
    assert_eq!(st.node.size(), (64, 36));
}

#[test]
fn bad_still_settings_are_refused() {
    let reg = SourceTypes::new();
    let refuse = |type_id: &str, pairs: &[(&str, SettingValue)]| {
        reg.instantiate(&source(type_id, settings(pairs)), &ctx())
            .unwrap_err()
    };
    // An image needs a path or a sample, and a known one.
    assert!(matches!(
        refuse(types::IMAGE_SOURCE, &[]),
        SourceError::InvalidSetting { key: "path", .. }
    ));
    assert!(matches!(
        refuse(types::IMAGE_SOURCE, &[(keys::SAMPLE, text("nope"))]),
        SourceError::InvalidSetting { key: "sample", .. }
    ));
    assert!(matches!(
        refuse(
            types::IMAGE_SOURCE,
            &[(keys::PATH, text("/no/such/file.png"))]
        ),
        SourceError::Image(_)
    ));
    assert!(matches!(
        refuse(types::COLOR_SOURCE, &[(keys::COLOR, text("red"))]),
        SourceError::InvalidSetting { key: "color", .. }
    ));
    assert!(matches!(
        refuse(types::COLOR_SOURCE, &[(keys::WIDTH, SettingValue::Int(8))]),
        SourceError::InvalidSetting { key: "width", .. }
    ));
    assert!(matches!(
        refuse(
            types::COLOR_SOURCE,
            &[
                (keys::WIDTH, SettingValue::Int(100_000)),
                (keys::HEIGHT, SettingValue::Int(8))
            ]
        ),
        SourceError::InvalidSetting { key: "width", .. }
    ));
}

#[test]
fn every_built_in_look_is_accepted_by_the_filter_catalogue() {
    for look in rbs_scene::look::built_in() {
        for f in &look.filters {
            let ty = rbs_filters::find(f.type_id)
                .unwrap_or_else(|| panic!("{}: no filter type {}", look.id, f.type_id));
            rbs_sources::filter_values(ty, &f.settings)
                .unwrap_or_else(|e| panic!("{}: {e}", look.id));
        }
    }
}

#[test]
fn source_errors_map_to_faults() {
    use rbs_audio_io::AudioIoError;
    use rbs_capture::CaptureError;
    use rbs_core::fault::SourceFault;
    use rbs_sources::SourceError;
    assert_eq!(
        SourceError::Capture(CaptureError::TargetLost).fault(),
        SourceFault::TargetGone
    );
    assert_eq!(
        SourceError::Audio(AudioIoError::EndpointNotFound).fault(),
        SourceFault::TargetGone
    );
    assert_eq!(
        SourceError::Audio(AudioIoError::DeviceBusy).fault(),
        SourceFault::Busy
    );
    assert_eq!(
        SourceError::Audio(AudioIoError::PermissionNotDetermined).fault(),
        SourceFault::PermissionDenied
    );
    assert_eq!(
        SourceError::Image(String::new()).fault(),
        SourceFault::Other
    );
}

#[test]
fn window_targets_carry_the_owning_application() {
    let (reg, _) = registry();
    let list = reg.targets(&ty(types::WINDOW_SOURCE)).unwrap();
    let window = list.iter().find(|t| t.target == "test-window-0").unwrap();
    assert_eq!(window.app.as_deref(), Some("Test application"));
}

#[test]
fn settings_are_validated_for_the_type_without_touching_a_device() {
    use rbs_sources::validate_settings;
    let camera = ty(types::CAMERA_SOURCE);
    let ok = settings(&[
        (keys::WIDTH, SettingValue::Int(1280)),
        (keys::HEIGHT, SettingValue::Int(720)),
        (keys::FPS, SettingValue::Int(30)),
    ]);
    assert!(validate_settings(&camera, &ok).is_ok());
    let cases = [
        settings(&[(keys::WIDTH, SettingValue::Int(1280))]),
        settings(&[(keys::FPS, SettingValue::Int(0))]),
        settings(&[(keys::FPS, SettingValue::Int(1000))]),
        settings(&[(keys::FPS, text("fast"))]),
        settings(&[(keys::CURSOR, SettingValue::Bool(true))]),
        settings(&[("bogus", SettingValue::Int(1))]),
    ];
    for bad in cases {
        assert!(validate_settings(&camera, &bad).is_err(), "{bad:?}");
    }
    let screen = ty(types::DISPLAY_SOURCE);
    assert!(
        validate_settings(
            &screen,
            &settings(&[(keys::CURSOR, SettingValue::Bool(false))])
        )
        .is_ok()
    );
    assert!(validate_settings(&ty("org.rbs.nope"), &Settings::new()).is_err());
}
