//! Add source from the window and from a client (ADR-0168 step 4): the window's
//! `source.add` asks the OS permission first and adds once it is answered; a
//! client's never prompts.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_audio_io_test::TestAudioBackend;
use rbs_capture::{Permission, TargetKind};
use rbs_capture_test::TestCaptureFactory;
use rbs_core::time::TestClock;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_record::RecordAdapters;
use rbs_studio_host::studio_host::{HostConfig, SourceKind, StudioHost, Update};
use serde_json::json;

/// Hang guard only.
const LIMIT: Duration = Duration::from_secs(20);

fn gpu() -> Option<Arc<GpuContext>> {
    static CTX: OnceLock<Option<Arc<GpuContext>>> = OnceLock::new();
    CTX.get_or_init(|| {
        match GpuContext::new(&GpuRequest {
            allow_software: true,
            ..GpuRequest::default()
        }) {
            Ok(ctx) => Some(Arc::new(ctx)),
            Err(GpuError::NoAdapter) => None,
            Err(e) => panic!("{e}"),
        }
    })
    .clone()
}

#[test]
fn a_window_add_prompts_first_and_a_client_add_never_does() {
    let Some(gpu) = gpu() else { return };
    let capture = Arc::new(TestCaptureFactory::new().with_permission(
        TargetKind::Camera,
        Permission::NotDetermined,
        Some(Permission::Granted),
        false,
    ));
    let factory = Arc::clone(&capture);
    let (tx, rx) = mpsc::channel();
    let mut config = HostConfig::desktop();
    config.preview = None;
    let host = StudioHost::spawn(
        config,
        move || {
            let mut a = RecordAdapters::new();
            a.sources.register_capture(factory).unwrap();
            a.sources
                .register_audio(Arc::new(TestAudioBackend::new()))
                .unwrap();
            (a, Vec::new())
        },
        move || Ok(gpu),
        || {
            Engine::with_clock(
                Arc::new(TestClock::new()),
                EngineConfig::default().swap_timeout(LIMIT),
            )
        },
        crate::watch::forward(tx),
    )
    .unwrap();
    loop {
        match rx.recv_timeout(LIMIT).expect("the host started") {
            Update::Started { .. } => break,
            Update::Failed(e) => panic!("{e}"),
            _ => {}
        }
    }
    let cameras = crate::listed(&host, &rx, SourceKind::Camera);

    // A client adds without a prompt, whatever the OS says.
    let mut c = crate::record::ApiClient::attach(&host);
    let added = c.call(
        "source.add",
        json!({"kind": "camera", "adapter": cameras[0].adapter, "target": cameras[0].target}),
    );
    assert!(added.get("result").is_some(), "{added}");
    assert_eq!(capture.prompts(), 0, "a client's source.add prompted");

    // The window asks first, then adds once the prompt is answered.
    let other = cameras.last().unwrap();
    host.send(crate::add_source(SourceKind::Camera, other));
    loop {
        match rx
            .recv_timeout(LIMIT)
            .expect("the window's camera was added")
        {
            Update::Sources(rows)
                if rows
                    .iter()
                    .filter(|r| r.kind == Some(SourceKind::Camera))
                    .count()
                    >= 2 =>
            {
                break;
            }
            Update::Error(e) => panic!("{e}"),
            _ => {}
        }
    }
    assert_eq!(capture.prompts(), 1, "the window's add asked once");
    drop(host);
    while rx.recv_timeout(LIMIT).is_ok() {}
}
