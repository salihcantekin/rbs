//! One command path (ADR-0163): the window's control API commands answer as asked,
//! the window-only views and previews change no studio state, and the commands that
//! translate to API methods run only the methods they declare (decision 5). The
//! reference adapters on a `TestClock`; waits end on the awaited update, with a
//! bound only as a hang guard (ADR-0032). Needs a GPU adapter (software is fine).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::cell::Cell;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_api::request::VerticalLayout;
use rbs_api::{ApiError, Response};
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_core::media::Size;
use rbs_core::time::TestClock;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_record::RecordAdapters;
use rbs_scene::model::{FilterId, SceneId, SourceId};
use rbs_studio_host::studio_host::{
    ApiReply, ApiTrace, Command, CommandPath, HostConfig, OutputCanvas, SourceKind, StudioHost,
    Target, Update, WindowReason,
};

/// Hang guard only: every wait ends as soon as its update arrives.
const LIMIT: Duration = Duration::from_secs(20);

fn gpu() -> Option<Arc<GpuContext>> {
    // One device per test binary: software Vulkan drivers crash with several.
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

/// A started host on the reference adapters, its updates and its API trace.
struct Studio {
    host: StudioHost,
    rx: Receiver<Update>,
    trace: ApiTrace,
    ticket: Cell<u64>,
}

impl Studio {
    fn start(gpu: Arc<GpuContext>) -> Self {
        let trace = ApiTrace::default();
        let mut config = HostConfig::desktop();
        config.preview = None;
        config.api_trace = Some(Arc::clone(&trace));
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
        let studio = Self {
            host,
            rx,
            trace,
            ticket: Cell::new(0),
        };
        studio.next(|u| match u {
            Update::Started { .. } => Some(()),
            Update::Failed(e) => panic!("{e}"),
            _ => None,
        });
        studio
    }

    /// The next update `pick` accepts; the ones skipped are given to `seen`.
    fn next_seeing<T>(
        &self,
        mut pick: impl FnMut(&Update) -> Option<T>,
        mut seen: impl FnMut(&Update),
    ) -> T {
        let deadline = std::time::Instant::now() + LIMIT;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let u = self.rx.recv_timeout(left).expect("the host answered");
            if let Some(t) = pick(&u) {
                return t;
            }
            seen(&u);
        }
    }

    fn next<T>(&self, pick: impl FnMut(&Update) -> Option<T>) -> T {
        self.next_seeing(pick, |_| {})
    }

    fn api(&self, call: rbs_api::Command, reply: ApiReply) {
        self.host.send(Command::Api {
            call: Box::new(call),
            reply,
        });
    }

    /// Runs `call` with a ticket and waits for its answer; the updates before it go to
    /// `seen`.
    fn ask_seeing(
        &self,
        call: rbs_api::Command,
        seen: impl FnMut(&Update),
    ) -> Result<Response, ApiError> {
        let ticket = self.ticket.get() + 1;
        self.ticket.set(ticket);
        self.api(call, ApiReply::Ticket(ticket));
        self.next_seeing(
            |u| match u {
                Update::ApiAnswer { ticket: t, result } if *t == ticket => Some(result.clone()),
                _ => None,
            },
            seen,
        )
    }

    fn ask(&self, call: rbs_api::Command) -> Result<Response, ApiError> {
        self.ask_seeing(call, |_| {})
    }

    /// What the window and a client can read of the studio: its scenes, sources and
    /// settings, each with its `seq`.
    fn state(&self) -> Vec<Response> {
        [
            rbs_api::Command::SceneList,
            rbs_api::Command::SourceList,
            rbs_api::Command::SettingsGet,
        ]
        .into_iter()
        .map(|c| self.ask(c).unwrap())
        .collect()
    }

    fn add_screen(&self) -> SourceId {
        let targets: Vec<Target> = crate::listed(&self.host, &self.rx, SourceKind::Screen);
        self.host
            .send(crate::add_source(SourceKind::Screen, &targets[0].clone()));
        self.next(|u| match u {
            Update::Sources(rows) if !rows.is_empty() => Some(rows[0].id),
            _ => None,
        })
    }

    fn program(&self) -> SceneId {
        match self.ask(rbs_api::Command::SceneList).unwrap() {
            Response::Scenes(s) => SceneId::from_raw(s.data.program),
            r => panic!("{r:?}"),
        }
    }
}

/// The variants of `path` in [`Command::paths`].
fn named(path: impl Fn(&CommandPath) -> bool) -> Vec<&'static str> {
    let mut names: Vec<_> = Command::paths()
        .iter()
        .filter(|(_, p)| path(p))
        .map(|(n, _)| *n)
        .collect();
    names.sort_unstable();
    names
}

fn names_of(samples: &[Command]) -> Vec<&'static str> {
    let mut names: Vec<_> = samples.iter().map(Command::name).collect();
    names.sort_unstable();
    names.dedup();
    names
}

#[test]
fn window_api_commands_answer_as_the_window_asked() {
    let Some(gpu) = gpu() else { return };
    let s = Studio::start(gpu);

    // A ticket carries the result.
    assert!(matches!(
        s.ask(rbs_api::Command::SceneList),
        Ok(Response::Scenes(_))
    ));

    // A refusal shows as the error bar and still answers the ticket.
    let rename = || {
        rbs_api::Command::SceneRename(rbs_api::request::RenameScene {
            scene: 9999,
            name: "x".into(),
        })
    };
    let mut errors = Vec::new();
    let answer = s.ask_seeing(rename(), |u| {
        if let Update::Error(e) = u {
            errors.push(e.clone());
        }
    });
    let refused = answer.unwrap_err();
    assert_eq!(errors, std::slice::from_ref(&refused.message));

    // Shown: the refusal shows, with no answer of its own.
    s.api(rename(), ApiReply::Shown);
    let shown = s.next(|u| match u {
        Update::Error(e) => Some(e.clone()),
        Update::ApiAnswer { .. } => panic!("no ticket, no answer"),
        _ => None,
    });
    assert_eq!(shown, refused.message);

    // Quiet: nothing to undo is not shown.
    s.api(rbs_api::Command::EditUndo, ApiReply::Quiet);
    s.ask_seeing(rbs_api::Command::SceneList, |u| {
        assert!(!matches!(u, Update::Error(_)), "{u:?}");
    })
    .unwrap();
}

#[test]
fn window_only_views_and_previews_change_no_studio_state() {
    let Some(gpu) = gpu() else { return };
    let s = Studio::start(gpu);
    let screen = s.add_screen();
    s.host.send(Command::Api {
        call: Box::new(rbs_api::Command::FilterAdd(rbs_api::request::AddFilter {
            source: screen.get(),
            type_id: "org.rbs.filter.sharpen".into(),
            settings: None,
        })),
        reply: ApiReply::Shown,
    });
    let filter: FilterId = s.next(|u| match u {
        Update::Sources(rows) => rows
            .iter()
            .find(|r| r.id == screen)
            .and_then(|r| r.filters.first())
            .map(|f| f.id),
        _ => None,
    });
    let values = rbs_params::Values::defaults(rbs_filters::find("org.rbs.filter.sharpen").unwrap());

    let samples = [
        Command::OpenSourceSettings(screen),
        Command::CloseSourceSettings,
        Command::OpenAttributes {
            source: screen,
            paste: false,
        },
        Command::OpenSyncMeasure(screen),
        Command::GrabThumbnails {
            kind: SourceKind::Screen,
            rows: Vec::new(),
            max: (64, 36),
        },
        Command::StopThumbnails,
        Command::SetMetersUi(true),
        Command::SetMetersUi(false),
        Command::EnsureEncoders,
        Command::OpenProjector {
            id: 7,
            canvas: OutputCanvas::Main,
        },
        Command::CloseProjector(7),
        Command::OpenInteract(screen),
        Command::CloseInteract(screen),
        Command::SetVerticalPicture(true),
        Command::SetVerticalPicture(false),
        Command::PreviewFilter { filter, values },
        Command::RestoreFilter(filter),
    ];
    let expected = named(|p| {
        matches!(
            p,
            CommandPath::WindowOnly(WindowReason::View | WindowReason::Preview)
        )
    });
    assert_eq!(names_of(&samples), expected, "a sample for each, ADR-0163");

    // A view that makes sources wanted (a projector, an Interact window, the vertical
    // picture) changes what runs, so `source.changed` counts for it; nothing else.
    let runs_sources = [
        "OpenProjector",
        "CloseProjector",
        "OpenInteract",
        "CloseInteract",
        "SetVerticalPicture",
    ];
    let mut before = stored(s.state(), false);
    for c in samples {
        let name = c.name();
        s.host.send(c);
        // Wait for the round to end; nothing on the history meanwhile.
        s.ask_seeing(rbs_api::Command::SceneList, |u| {
            assert!(!matches!(u, Update::History { .. }), "{name}: {u:?}");
        })
        .unwrap();
        let now = stored(s.state(), runs_sources.contains(&name));
        assert_eq!(
            now,
            stored(before.clone(), runs_sources.contains(&name)),
            "{name} changed the studio"
        );
        before = stored(s.state(), false);
    }
}

/// `state` without what running changes: a source's status, picture and input
/// counters, and with `running` the `source.changed` count too.
fn stored(mut state: Vec<Response>, running: bool) -> Vec<Response> {
    for r in &mut state {
        if let Response::Sources(list) = r {
            if running {
                list.seq.clear();
            }
            for i in &mut list.data {
                i.status = rbs_api::types::SourceStatus::Idle;
                i.picture_size = None;
                i.input_late_frames = None;
                i.input_resyncs = None;
                i.warm_up_ms = None;
            }
        }
    }
    state
}

#[test]
fn translating_commands_run_only_their_declared_methods() {
    let Some(gpu) = gpu() else { return };
    let s = Studio::start(gpu);
    let screen = s.add_screen();
    let scene = s.program();
    let samples = [
        Command::SetSourceVolume {
            id: screen,
            volume: 0.5,
        },
        Command::SetSourceMuted {
            id: screen,
            muted: true,
        },
        Command::SetSceneOwnLevels { scene, own: true },
        Command::ResetSceneLevel {
            scene,
            source: screen,
        },
        Command::SetSceneOwnLevels { scene, own: false },
        Command::AutoLayoutVertical(VerticalLayout::Fit),
        Command::SetVerticalSize(Size::new(1080, 1920)),
        Command::SetMasterVolume { volume: 0.8 },
        Command::SetMasterMuted { muted: true },
        Command::SetLoudnessShown(true),
        Command::SetLoudnessSettings {
            target: -14.0,
            true_peak_limit: -1.0,
        },
        Command::SetSourceMixerHidden {
            id: screen,
            hidden: true,
        },
        Command::ApplyLook {
            source: screen,
            look: "builtin:none".into(),
        },
        Command::AddSample {
            sample: "none".into(),
        },
        Command::AddMarker,
        Command::RemoveMarkers(vec![(OutputCanvas::Main, 0)]),
        Command::StartDestination(0),
        Command::StopDestination(0),
        Command::PauseRecording,
        Command::HoldStream,
        Command::Break,
        Command::SetStudioMode(true),
        Command::SwitchScene(scene),
        Command::SetStudioMode(false),
        Command::SwitchScene(scene),
        Command::CloseSyncMeasure,
        Command::BrowserControl {
            id: screen,
            message: rbs_browser_ipc::control::Control::Reload {
                bypass_cache: false,
            },
        },
    ];
    let expected = named(|p| matches!(p, CommandPath::Translates(_)));
    assert_eq!(names_of(&samples), expected, "a sample for each, ADR-0163");

    for c in samples {
        let (name, CommandPath::Translates(declared)) = (c.name(), c.path()) else {
            unreachable!()
        };
        s.trace.lock().unwrap().clear();
        s.host.send(c);
        s.ask(rbs_api::Command::SceneList).unwrap();
        let mut ran = std::mem::take(&mut *s.trace.lock().unwrap());
        assert_eq!(ran.pop(), Some("scene.list"), "{name}: the barrier last");
        // With no measurement running, closing the Measure window has nothing to
        // cancel, so it runs no method (ADR-0168 decision 1).
        if name != "CloseSyncMeasure" {
            assert!(!ran.is_empty(), "{name} ran no API method");
        }
        for m in ran {
            assert!(declared.contains(&m), "{name} ran {m}, not in {declared:?}");
        }
    }
}

/// Step 1 (scenes and transitions): the window's scene edits are API calls, each one
/// undo step as before.
#[test]
fn scene_edits_from_the_window_are_api_calls_and_undo_steps() {
    let Some(gpu) = gpu() else { return };
    let s = Studio::start(gpu);
    let names = |s: &Studio| match s.ask(rbs_api::Command::SceneList).unwrap() {
        Response::Scenes(l) => l
            .data
            .scenes
            .into_iter()
            .map(|i| i.name)
            .collect::<Vec<_>>(),
        r => panic!("{r:?}"),
    };
    let Response::SceneAdded(added) = s
        .ask(rbs_api::Command::SceneAdd(rbs_api::request::AddScene {
            name: None,
        }))
        .unwrap()
    else {
        panic!("scene.add answers SceneAdded")
    };
    s.ask(rbs_api::Command::SceneRename(
        rbs_api::request::RenameScene {
            scene: added.scene,
            name: "Talk".into(),
        },
    ))
    .unwrap();
    s.ask(rbs_api::Command::SceneDuplicate(
        rbs_api::request::DuplicateScene {
            scene: added.scene,
            sources: rbs_api::types::DuplicateSources::Shared,
        },
    ))
    .unwrap();
    let three = names(&s);
    assert_eq!(three.len(), 3, "{three:?}");
    assert!(three.contains(&"Talk".to_owned()), "{three:?}");

    // Undo takes back the duplicate only.
    s.host.send(Command::Api {
        call: Box::new(rbs_api::Command::EditUndo),
        reply: rbs_studio_host::studio_host::ApiReply::QuietIf(rbs_api::ErrorKind::Conflict),
    });
    s.ask(rbs_api::Command::SceneList).unwrap();
    let two = names(&s);
    assert_eq!(two.len(), 2, "{two:?}");
    assert!(two.contains(&"Talk".to_owned()), "{two:?}");

    s.ask(rbs_api::Command::SceneRemove(rbs_api::request::SceneRef {
        scene: added.scene,
    }))
    .unwrap();
    assert_eq!(names(&s).len(), 1);
}

/// A window call that names no scene adds where the window edits: the preview scene
/// in studio mode (ADR-0061 decision 4), as the window's own commands did.
#[test]
fn a_window_addition_goes_to_the_preview_scene_in_studio_mode() {
    let Some(gpu) = gpu() else { return };
    let s = Studio::start(gpu);
    let screen = s.add_screen();
    let Response::SceneAdded(added) = s
        .ask(rbs_api::Command::SceneAdd(rbs_api::request::AddScene {
            name: None,
        }))
        .unwrap()
    else {
        panic!("scene.add answers SceneAdded")
    };
    s.ask(rbs_api::Command::CanvasSetPreview(
        rbs_api::request::SetPreview {
            scene: Some(added.scene),
        },
    ))
    .unwrap();
    assert_ne!(
        s.program().get(),
        added.scene,
        "the new scene is in preview only"
    );
    // Add source › Existing source: the Sources panel (the preview scene) shows it.
    let mut rows = None;
    s.ask_seeing(
        rbs_api::Command::ItemAdd(rbs_api::request::AddItem {
            source: screen.get(),
            scene: None,
        }),
        |u| {
            if let Update::Sources(r) = u {
                rows = Some(r.iter().map(|r| r.id).collect::<Vec<_>>());
            }
        },
    )
    .unwrap();
    assert_eq!(rows, Some(vec![screen]), "added to the preview scene");
}
