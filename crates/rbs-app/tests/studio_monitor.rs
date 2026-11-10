//! The monitor output through the host (ADR-0097 F1): a collection with a monitored
//! source opens the monitor device on the host's control thread, and the device plays
//! the mixer's monitor pin. Reference adapters, an engine on a `TestClock`; waits end
//! when a count is reached, with a bound only as a hang guard (ADR-0032). Needs a GPU
//! adapter (software is fine); skipped without one unless `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rbs_app::studio_host::{HostConfig, StudioHost, Update};
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_core::media::Size;
use rbs_core::time::TestClock;
use rbs_core::timing::FrameRate;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_graph::node::NodeTypeId;
use rbs_record::RecordAdapters;
use rbs_scene::SceneCollection;
use rbs_scene::model::{ItemContent, Layout, SceneScope, Settings, SourceMonitor};
use rbs_sources::types;

/// Hang guard only.
const LIMIT: Duration = Duration::from_secs(20);

fn gpu() -> Option<Arc<GpuContext>> {
    static CTX: OnceLock<Option<Arc<GpuContext>>> = OnceLock::new();
    let ctx = CTX.get_or_init(|| {
        match GpuContext::new(&GpuRequest {
            allow_software: true,
            ..GpuRequest::default()
        }) {
            Ok(ctx) => Some(Arc::new(ctx)),
            Err(GpuError::NoAdapter) => None,
            Err(e) => panic!("{e}"),
        }
    });
    if ctx.is_none() {
        assert!(
            std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
            "RBS_REQUIRE_GPU is set but no adapter was found"
        );
        eprintln!("SKIPPED: no GPU adapter");
    }
    ctx.clone()
}

/// One canvas whose program scene has a microphone with `monitor`.
fn collection(monitor: SourceMonitor) -> SceneCollection {
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_30)
        .unwrap();
    let (scene, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    let (mic, _) = col
        .add_source(
            "Microphone",
            NodeTypeId::new_static(types::AUDIO_INPUT_SOURCE),
            Settings::new(),
        )
        .unwrap();
    col.set_source_monitor(mic, monitor).unwrap();
    col.add_item(
        scene,
        ItemContent::Source(mic),
        Layout::new([0.0, 0.0], [1.0, 1.0]),
        None,
    )
    .unwrap();
    col.set_program(canvas, Some(scene)).unwrap();
    col
}

fn start(monitor: SourceMonitor) -> Option<(StudioHost, Receiver<Update>, Arc<TestAudioBackend>)> {
    let gpu = gpu()?;
    let (tx, updates) = mpsc::channel();
    let audio = Arc::new(TestAudioBackend::new());
    let backend = Arc::clone(&audio);
    let mut config = HostConfig::desktop();
    config.preview = None;
    config.collection = Some(collection(monitor));
    let host = StudioHost::spawn(
        config,
        move || {
            let mut a = RecordAdapters::new();
            a.sources
                .register_capture(Arc::new(TestCaptureFactory::new()))
                .unwrap();
            a.sources.register_audio(backend).unwrap();
            (a, Vec::new())
        },
        move || Ok(gpu),
        || {
            Engine::with_clock(
                Arc::new(TestClock::new()),
                EngineConfig::default().swap_timeout(LIMIT),
            )
        },
        move |u| {
            let _ = tx.send(u);
        },
    )
    .unwrap();
    let deadline = Instant::now() + LIMIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match updates.recv_timeout(left).expect("the host started") {
            Update::Started { .. } => break,
            Update::Failed(e) => panic!("{e}"),
            _ => {}
        }
    }
    Some((host, updates, audio))
}

/// Waits (hang guard only) until `audio` has `streams` active streams.
fn streams_reach(audio: &TestAudioBackend, streams: usize) -> bool {
    let deadline = Instant::now() + LIMIT;
    while Instant::now() < deadline {
        if audio.active_streams() == streams {
            return true;
        }
        std::thread::yield_now();
    }
    false
}

#[test]
fn a_monitored_source_opens_the_monitor_device() {
    let Some((host, _updates, audio)) = start(SourceMonitor::Only) else {
        return;
    };
    // The microphone's input and the monitor's output.
    assert!(
        streams_reach(&audio, 2),
        "{} streams",
        audio.active_streams()
    );
    drop(host);
    assert!(streams_reach(&audio, 0));
}

#[test]
fn without_a_monitored_source_no_device_opens() {
    let Some((host, _updates, audio)) = start(SourceMonitor::Off) else {
        return;
    };
    // The microphone runs; nothing else opens.
    assert!(streams_reach(&audio, 1));
    drop(host);
    assert!(streams_reach(&audio, 0));
}
