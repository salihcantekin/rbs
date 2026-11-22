//! Projectors in the studio host (ADR-0125 decisions 2 to 4): a projector's picture at
//! the canvas's size, added again after a restart, suspended while its canvas is gone,
//! and the limits. The engine runs on a `TestClock` that never moves; the tests wait
//! for the host's updates, with a bound only as a hang guard (ADR-0032). Pictures need
//! a preview backend (macOS, Windows) and a GPU adapter; skipped without one unless
//! `RBS_REQUIRE_GPU` is set. The limit rule is pure and runs everywhere.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_core::media::Size;
use rbs_studio_host::studio_host::{
    MAX_PROJECTOR_SURFACE_BYTES, MAX_PROJECTORS, projector_refusal, projector_surface_bytes,
};

#[test]
fn projector_limits_count_eight_surfaces_per_projector_on_every_platform() {
    let hd = Size::new(1920, 1080);
    let uhd = Size::new(3840, 2160);
    assert_eq!(projector_surface_bytes(hd), 1920 * 1080 * 4 * 8);
    // Four 1080p projectors fit; a fifth is refused by count.
    let mut open = 0;
    for n in 0..MAX_PROJECTORS {
        assert_eq!(projector_refusal(n, open, hd), None, "projector {n}");
        open += projector_surface_bytes(hd);
    }
    assert!(open <= MAX_PROJECTOR_SURFACE_BYTES);
    assert!(projector_refusal(MAX_PROJECTORS, open, hd).is_some());
    // Two 2160p projectors fit; a third is refused by memory.
    let two = 2 * projector_surface_bytes(uhd);
    assert_eq!(
        projector_refusal(1, projector_surface_bytes(uhd), uhd),
        None
    );
    assert!(projector_refusal(2, two, uhd).is_some());
}

#[cfg(any(windows, target_vendor = "apple"))]
mod with_pictures {
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    use rbs_api::types::OutputCanvas;
    use rbs_audio_io_test::TestAudioBackend;
    use rbs_capture_test::TestCaptureFactory;
    use rbs_core::media::Size;
    use rbs_core::time::TestClock;
    use rbs_core::timing::FrameRate;
    use rbs_engine::{Engine, EngineConfig};
    use rbs_gpu::share::PreviewEndpoint;
    use rbs_gpu::{GpuContext, GpuError, GpuRequest};
    use rbs_record::RecordAdapters;
    use rbs_studio_host::studio_host::{
        Command, HostConfig, ProjectorState, StudioHost, Update, VerticalCanvas,
    };

    /// Hang guard only: every wait ends as soon as its update arrives.
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
            rbs_crash::errln!("SKIPPED: no GPU adapter");
        }
        ctx.clone()
    }

    fn host(gpu: Arc<GpuContext>) -> (StudioHost, Receiver<Update>) {
        let mut config = HostConfig::desktop();
        config.preview = None;
        let (tx, rx) = mpsc::channel();
        let host = StudioHost::spawn(
            config,
            || {
                let mut a = RecordAdapters::new();
                a.sources
                    .register_capture(Arc::new(TestCaptureFactory::new()))
                    .unwrap();
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
            move |u| {
                let _ = tx.send(u);
            },
        )
        .unwrap();
        next(&rx, |u| match u {
            Update::Started { .. } => Some(()),
            Update::Failed(e) => panic!("{e}"),
            _ => None,
        });
        (host, rx)
    }

    /// The next update `pick` accepts; others are skipped.
    fn next<T>(rx: &Receiver<Update>, mut pick: impl FnMut(Update) -> Option<T>) -> T {
        let deadline = std::time::Instant::now() + LIMIT;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let u = rx.recv_timeout(left).expect("the host answered");
            if let Some(t) = pick(u) {
                return t;
            }
        }
    }

    /// The next state of projector `id` that `ok` accepts, with its picture.
    fn projector(
        rx: &Receiver<Update>,
        id: u32,
        mut ok: impl FnMut(&ProjectorState, &Option<PreviewEndpoint>) -> bool,
    ) -> (ProjectorState, Option<PreviewEndpoint>) {
        next(rx, |u| match u {
            Update::Projector {
                id: i,
                picture,
                state,
            } if i == id && ok(&state, &picture) => Some((state, picture)),
            Update::Failed(e) => panic!("{e}"),
            _ => None,
        })
    }

    fn open_with_picture(s: &ProjectorState, p: &Option<PreviewEndpoint>) -> bool {
        *s == ProjectorState::Open && p.is_some()
    }

    #[test]
    fn a_projector_of_the_main_canvas_comes_back_after_a_restart() {
        let Some(gpu) = gpu() else { return };
        let (host, rx) = host(gpu);
        host.send(Command::OpenProjector {
            id: 1,
            canvas: OutputCanvas::Main,
        });
        let (_, first) = projector(&rx, 1, open_with_picture);
        // A canvas change restarts the studio: the projector gets a new picture.
        host.send(crate::set_canvas(Size::new(1280, 720), FrameRate::FPS_30));
        let (_, again) = projector(&rx, 1, |s, p| open_with_picture(s, p) && *p != first);
        assert_ne!(again, first);
        host.send(Command::CloseProjector(1));
        // Closing an unknown projector changes nothing.
        host.send(Command::CloseProjector(77));
    }

    #[test]
    fn a_projector_of_the_vertical_canvas_waits_for_it() {
        let Some(gpu) = gpu() else { return };
        let (host, rx) = host(gpu);
        host.send(Command::OpenProjector {
            id: 2,
            canvas: OutputCanvas::Vertical,
        });
        // No vertical canvas: suspended, no picture (ADR-0125 decision 3).
        let (_, picture) = projector(&rx, 2, |s, _| *s == ProjectorState::Suspended);
        assert!(picture.is_none());
        host.send(Command::Api {
            call: Box::new(rbs_api::Command::CanvasAddVertical(
                rbs_api::request::AddVerticalCanvas {
                    width: None,
                    height: None,
                    follow: None,
                    layout: None,
                },
            )),
            reply: rbs_studio_host::studio_host::ApiReply::Shown,
        });
        projector(&rx, 2, open_with_picture);
        host.send(Command::Api {
            call: Box::new(rbs_api::Command::CanvasRemoveVertical(Default::default())),
            reply: rbs_studio_host::studio_host::ApiReply::Shown,
        });
        let (_, picture) = projector(&rx, 2, |s, _| *s == ProjectorState::Suspended);
        assert!(picture.is_none());
    }

    #[test]
    fn a_fifth_projector_is_refused() {
        let Some(gpu) = gpu() else { return };
        let (host, rx) = host(gpu);
        for id in 0..4 {
            host.send(Command::OpenProjector {
                id,
                canvas: OutputCanvas::Main,
            });
            projector(&rx, id, open_with_picture);
        }
        host.send(Command::OpenProjector {
            id: 4,
            canvas: OutputCanvas::Main,
        });
        let (state, picture) = projector(&rx, 4, |s, _| matches!(s, ProjectorState::Closed(_)));
        assert!(picture.is_none());
        assert!(matches!(state, ProjectorState::Closed(why) if why.contains("Too many")));
        // Closing one makes room.
        host.send(Command::CloseProjector(0));
        host.send(Command::OpenProjector {
            id: 4,
            canvas: OutputCanvas::Main,
        });
        projector(&rx, 4, open_with_picture);
    }
}
