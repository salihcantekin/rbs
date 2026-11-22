//! The studio host's integration tests, one binary (ADR-0159 decision 7): each module
//! was its own `rbs-app` test file and keeps its own helpers.

mod add_ons;
mod add_source;
mod api;
mod attributes;
mod chapters;
mod command_path;
mod faults;
mod filters;
mod host;
mod loudness;
mod monitor;
mod projector;
mod record;
mod replay;
mod scenes;
#[cfg(all(windows, target_arch = "x86_64"))]
mod stacks;
mod stream;
mod sync_offset;
mod vcam;
mod watch;

/// `source.set_settings` as the window sends it (ADR-0164): one undo step, or part of
/// `gesture`'s.
fn set_settings(
    id: rbs_scene::model::SourceId,
    settings: std::collections::BTreeMap<String, serde_json::Value>,
    gesture: u64,
) -> rbs_studio_host::studio_host::Command {
    rbs_studio_host::studio_host::Command::Api {
        call: Box::new(rbs_api::Command::SourceSetSettings(
            rbs_api::request::SetSourceSettings {
                source: id.get(),
                settings,
                gesture,
            },
        )),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    }
}

/// `item.set_style` as the window sends it (ADR-0164).
fn set_style(
    item: u32,
    style: &rbs_scene::model::ItemStyle,
    canvas: rbs_studio_host::studio_host::OutputCanvas,
) -> rbs_studio_host::studio_host::Command {
    rbs_studio_host::studio_host::Command::Api {
        call: Box::new(rbs_api::Command::ItemSetStyle(
            rbs_api::request::SetItemStyle {
                item,
                style: rbs_studio_host::studio_host::style_for_api(style),
                canvas,
                gesture: 0,
            },
        )),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    }
}

/// `item.set_scroll` as the window sends it (ADR-0164).
fn set_scroll(
    item: u32,
    scroll: Option<rbs_scene::model::Scroll>,
    canvas: rbs_studio_host::studio_host::OutputCanvas,
) -> rbs_studio_host::studio_host::Command {
    rbs_studio_host::studio_host::Command::Api {
        call: Box::new(rbs_api::Command::ItemSetScroll(
            rbs_api::request::SetItemScroll {
                item,
                scroll: scroll
                    .as_ref()
                    .map(rbs_studio_host::studio_host::scroll_for_api),
                canvas,
                gesture: 0,
            },
        )),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    }
}

/// Add source › Browser as the window sends it (ADR-0164 step 3), answered as `reply`.
fn add_browser(
    url: &str,
    reply: rbs_studio_host::studio_host::ApiReply,
) -> rbs_studio_host::studio_host::Command {
    rbs_studio_host::studio_host::Command::Api {
        call: Box::new(rbs_api::Command::SourceAdd(rbs_api::request::AddSource {
            kind: rbs_api::types::SourceKind::Browser,
            adapter: String::new(),
            target: String::new(),
            name: None,
            scene: None,
            global: false,
            with_audio: false,
            text: None,
            stamp: None,
            path: None,
            url: Some(url.to_owned()),
        })),
        reply,
    }
}

/// A control API call as the window sends it, quiet: a refused output start comes back
/// as the start's own update (ADR-0164 decision 3).
fn window_call(call: rbs_api::Command) -> rbs_studio_host::studio_host::Command {
    rbs_studio_host::studio_host::Command::Api {
        call: Box::new(call),
        reply: rbs_studio_host::studio_host::ApiReply::Quiet,
    }
}

/// The recording settings a test's explicit file is written with: `path`'s folder,
/// `encoder`, 6 Mbit/s constant and 128 kbit/s sound.
fn profile_into(
    path: &std::path::Path,
    encoder: Option<&str>,
) -> rbs_studio_host::record_profile::RecordProfile {
    let mut p = rbs_studio_host::record_profile::RecordProfile::new();
    if let Some(folder) = path.parent() {
        p.folder = folder.to_owned();
    }
    p.encoder = encoder.map(str::to_owned);
    p.rate_control = rbs_studio_host::record_profile::RateMode::Cbr;
    p.video_kbps = 6000;
    p.audio_kbps = 128;
    p
}

/// Records the main canvas into `path` with [`profile_into`]'s settings:
/// `record.start_to_file`, as a client names a file.
fn record_into(
    host: &rbs_studio_host::studio_host::StudioHost,
    path: std::path::PathBuf,
    encoder: Option<&str>,
) {
    use rbs_studio_host::studio_host::Command;
    host.send(Command::SetProfile(profile_into(&path, encoder)));
    host.send(record_file(&path));
}

/// `record.start_to_file` of the main canvas with the saved settings.
fn record_file(path: &std::path::Path) -> rbs_studio_host::studio_host::Command {
    window_call(rbs_api::Command::RecordStartToFile(
        rbs_api::request::RecordStartToFile {
            profile: rbs_studio_host::record_profile::API_PROFILE_ID.into(),
            path: path.display().to_string(),
            canvas: None,
        },
    ))
}

/// The window's Record with the separate files: `record.start` with `iso` into a
/// folder named as `path` without its extension (the take is named by the time,
/// to the second, so each take of a test gets a folder of its own).
fn record_with_tracks(
    host: &rbs_studio_host::studio_host::StudioHost,
    path: std::path::PathBuf,
    encoder: Option<&str>,
) {
    use rbs_studio_host::studio_host::Command;
    let into = path.with_extension("").join("take.mkv");
    host.send(Command::SetProfile(profile_into(&into, encoder)));
    host.send(record_start(rbs_api::types::OutputTarget::Main, Some(true)));
}

/// The window's Record of `canvas` with the saved settings.
fn record_start(
    canvas: rbs_api::types::OutputTarget,
    iso: Option<bool>,
) -> rbs_studio_host::studio_host::Command {
    window_call(rbs_api::Command::RecordStart(
        rbs_api::request::RecordStart {
            canvas: Some(canvas),
            iso,
            ..rbs_api::request::RecordStart::new(rbs_studio_host::record_profile::API_PROFILE_ID)
        },
    ))
}

/// The window's Go live of `canvas` with the saved stream settings.
fn go_live(canvas: rbs_api::types::OutputTarget) -> rbs_studio_host::studio_host::Command {
    window_call(rbs_api::Command::StreamStart(
        rbs_api::request::OnCanvases {
            canvas: Some(canvas),
        },
    ))
}

/// `item.set_crop` as the window sends it (ADR-0164 step 6): answered as `ticket`.
fn set_crop(
    item: u32,
    crop: [u32; 4],
    keep_scale: bool,
    canvas: rbs_studio_host::studio_host::OutputCanvas,
    gesture: u64,
    ticket: u64,
) -> rbs_studio_host::studio_host::Command {
    rbs_studio_host::studio_host::Command::Api {
        call: Box::new(rbs_api::Command::ItemSetCrop(
            rbs_api::request::SetItemCrop {
                item,
                crop,
                keep_scale,
                canvas,
                gesture,
            },
        )),
        reply: rbs_studio_host::studio_host::ApiReply::Ticket(ticket),
    }
}

/// Settings, Video's canvas as the window sends it: `settings.set {canvas}`
/// (ADR-0168 decision 2).
fn set_canvas(
    size: rbs_core::media::Size,
    fps: rbs_core::timing::FrameRate,
) -> rbs_studio_host::studio_host::Command {
    let r = fps.per_second();
    let patch = rbs_api::request::SettingsPatch {
        canvas: Some(rbs_api::types::Canvas {
            width: size.width,
            height: size.height,
            fps_num: r.num(),
            fps_den: r.den(),
        }),
        ..rbs_api::request::SettingsPatch::default()
    };
    rbs_studio_host::studio_host::Command::Api {
        call: Box::new(rbs_api::Command::SettingsSet(patch)),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    }
}

/// The camera's Stop as the window sends it: `vcam.stop`, quiet on `conflict`.
fn stop_camera() -> rbs_studio_host::studio_host::Command {
    rbs_studio_host::studio_host::Command::Api {
        call: Box::new(rbs_api::Command::VcamStop),
        reply: rbs_studio_host::studio_host::ApiReply::QuietIf(rbs_api::ErrorKind::Conflict),
    }
}

/// A box as the picture panel sends it (ADR-0168 step 2).
struct SetBox {
    seq: u64,
    gesture: u64,
    id: rbs_scene::model::SourceId,
    position: [f32; 2],
    size: [f32; 2],
    canvas: rbs_studio_host::studio_host::OutputCanvas,
}

/// `item.set_box` answered by the ticket `seq`, as the window sends a box.
fn set_box(b: SetBox) -> rbs_studio_host::studio_host::Command {
    rbs_studio_host::studio_host::Command::Api {
        call: Box::new(rbs_api::Command::ItemSetBox(rbs_api::request::SetItemBox {
            source: b.id.get(),
            position: b.position,
            size: b.size,
            gesture: b.gesture,
            seq: b.seq,
            canvas: b.canvas,
        })),
        reply: rbs_studio_host::studio_host::ApiReply::Ticket(b.seq),
    }
}

/// What `kind` can capture, asked as the window asks it: `source.targets` by ticket
/// (ADR-0168 step 4). Updates before the answer are skipped.
fn listed(
    host: &rbs_studio_host::studio_host::StudioHost,
    rx: &std::sync::mpsc::Receiver<rbs_studio_host::studio_host::Update>,
    kind: rbs_studio_host::studio_host::SourceKind,
) -> Vec<rbs_studio_host::studio_host::Target> {
    listed_seeing(host, rx, kind, |_| {})
}

/// [`listed`], showing `seen` each update before the answer.
#[allow(clippy::expect_used)] // Tests fail by panicking.
fn listed_seeing(
    host: &rbs_studio_host::studio_host::StudioHost,
    rx: &std::sync::mpsc::Receiver<rbs_studio_host::studio_host::Update>,
    kind: rbs_studio_host::studio_host::SourceKind,
    mut seen: impl FnMut(&rbs_studio_host::studio_host::Update),
) -> Vec<rbs_studio_host::studio_host::Target> {
    use rbs_studio_host::studio_host::{ApiReply, Command, Target, Update, kind_for_api};
    const TARGETS: u64 = u64::MAX - 3;
    let call = rbs_api::Command::SourceTargets(rbs_api::request::TargetsParams {
        kind: kind_for_api(kind).expect("a listed kind"),
    });
    host.send(Command::Api {
        call: Box::new(call),
        reply: ApiReply::Ticket(TARGETS),
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left).expect("the host answered") {
            Update::ApiAnswer {
                ticket: TARGETS,
                result,
            } => match result {
                Ok(rbs_api::Response::Targets(l)) => {
                    return l.targets.into_iter().map(Target::from_api).collect();
                }
                other => panic!("{other:?}"),
            },
            u => seen(&u),
        }
    }
}

/// Add source as the window sends it: `source.add` of a listed target, with its
/// sound when `audio` (ADR-0168 step 4).
#[allow(clippy::expect_used)] // Tests fail by panicking.
fn add_source_as(
    kind: rbs_studio_host::studio_host::SourceKind,
    target: &rbs_studio_host::studio_host::Target,
    audio: bool,
    global: bool,
) -> rbs_studio_host::studio_host::Command {
    rbs_studio_host::studio_host::Command::Api {
        call: Box::new(rbs_api::Command::SourceAdd(rbs_api::request::AddSource {
            kind: rbs_studio_host::studio_host::kind_for_api(kind).expect("a listed kind"),
            adapter: target.adapter.clone(),
            target: target.target.clone(),
            name: None,
            scene: None,
            global,
            with_audio: audio,
            text: None,
            stamp: None,
            path: None,
            url: None,
        })),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    }
}

/// [`add_source_as`] of the picture alone.
fn add_source(
    kind: rbs_studio_host::studio_host::SourceKind,
    target: &rbs_studio_host::studio_host::Target,
) -> rbs_studio_host::studio_host::Command {
    add_source_as(kind, target, false, false)
}
