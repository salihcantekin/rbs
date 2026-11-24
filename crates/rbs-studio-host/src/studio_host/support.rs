//! Small conversions and checks the concerns share.

use super::*;

/// Why a style is refused, with what to do.
pub(super) const STYLE_ERROR: &str = "Corner radius and border width must be numbers, not below 0, and a border must be wider than 0.";

/// A refused scene edit in words, with what to do.
pub(super) fn edit_error(e: &SceneError) -> String {
    match e {
        SceneError::StyleOnScene(_) => {
            "Shape and border are for sources, not for scenes placed in a scene.".to_owned()
        }
        SceneError::ScrollOnScene(_) => {
            "Scroll is for sources, not for scenes placed in a scene.".to_owned()
        }
        SceneError::InvalidLayout => {
            "Position and size must be numbers, and the size cannot be 0.".to_owned()
        }
        SceneError::UnknownItem(_) | SceneError::UnknownScene(_) | SceneError::UnknownSource(_) => {
            "This source is not in the scene any more.".to_owned()
        }
        SceneError::InvalidSyncOffset | SceneError::InvalidVolume => format!("{e}."),
        SceneError::ChromeNotDisplay => {
            "Only a display source can hide the taskbar or Dock.".to_owned()
        }
        SceneError::UnknownFilter(_) => "This filter is not there any more.".to_owned(),
        SceneError::StingerPlaced(_) => {
            "This source is the stinger, so it cannot be in a scene.".to_owned()
        }
        SceneError::UnknownGroup(_) => "This group is not there any more.".to_owned(),
        SceneError::GroupTooSmall(_) => "A group needs at least two sources.".to_owned(),
        SceneError::GroupNotContiguous(_) => {
            "A source of a group cannot be moved away from the group. Ungroup it first.".to_owned()
        }
        SceneError::GroupScope => "Sources of two scenes cannot be in one group.".to_owned(),
        other => format!("The scene could not be changed ({other:?}). Try again."),
    }
}

/// How long Fit box to text waits for the renderer. Adding a text source does not
/// wait: it uses the default box and [`text_measure`] patches the size later.
pub(super) const TEXT_MEASURE_WAIT: Duration = Duration::from_millis(200);

/// [`measure_text_wrapped`] at `max_width` (Fit box to text).
pub(super) fn measure_text_at(
    renderer: Arc<dyn rbs_text::TextRenderer>,
    block: rbs_text::TextBlock,
    max_width: Option<u32>,
) -> Option<[u32; 2]> {
    measure_text_wrapped(renderer, block, max_width, TEXT_MEASURE_WAIT)
}

/// Measures `block` with `renderer` on a thread of its own, waiting at most `wait`.
/// A late answer is dropped with its thread; a failure or a panic is `None`.
pub(super) fn measure_text_wrapped(
    renderer: Arc<dyn rbs_text::TextRenderer>,
    block: rbs_text::TextBlock,
    max_width: Option<u32>,
    wait: Duration,
) -> Option<[u32; 2]> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("rbs-text-measure".into())
        .spawn(move || {
            let measured = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                renderer.measure(&block, max_width)
            }));
            let _ = tx.send(measured.ok().and_then(Result::ok));
        })
        .ok()?;
    rx.recv_timeout(wait).ok().flatten()
}

/// One `media.*` method (protocol 1.83).
/// A playback action on a media source (ADR-0108 decision 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaOp {
    /// Play from the cursor.
    Play,
    /// Pause at the cursor.
    Pause,
    /// Move the cursor to this many milliseconds.
    Seek(u64),
    /// Play from the start.
    Restart,
    /// Read the state only.
    State,
    /// A slide show's next picture (ADR-0171 decision 9).
    Next,
    /// A slide show's previous picture.
    Previous,
}

/// `item.remove`'s answer (protocol 1.88, ADR-0142).
pub(super) fn item_removed(removed: &Removed) -> ItemRemoved {
    ItemRemoved {
        source_deleted: removed.source_deleted,
        removed_items: removed.removed_items.iter().map(|i| i.get()).collect(),
        deleted_sources: removed.deleted_sources.iter().map(|s| s.get()).collect(),
    }
}

/// A registry with only the encoder `id` of `all`, probed.
pub(super) fn encoder_catalog(adapters: &RecordAdapters) -> (Vec<EncoderInfo>, Option<String>) {
    let encoders: Vec<EncoderInfo> = adapters
        .video_encoders
        .select(&Requirement::new())
        .candidates
        .iter()
        .filter(|f| {
            adapters
                .video_encoders
                .capabilities(&f.id())
                .is_some_and(|c| !c.is_empty())
        })
        .map(|f| EncoderInfo {
            id: f.id().as_str().to_owned(),
            name: f.display_name().to_owned(),
            options: adapters
                .video_encoders
                .capabilities(&f.id())
                .map(EncoderOptions::from_caps)
                .unwrap_or_default(),
        })
        .collect();
    let cannot_record = encoders.is_empty().then(|| {
        format!(
            "Recording is not available on {} yet: this system has no video encoder RBS can use.",
            os_name()
        )
    });
    (encoders, cannot_record)
}

pub(super) fn only_encoder<F: ?Sized + AdapterFactory>(
    all: &Registry<F>,
    id: &str,
) -> Result<Registry<F>, Refusal> {
    let factory = all
        .select(&Requirement::new())
        .candidates
        .into_iter()
        .find(|f| f.id().as_str() == id)
        .ok_or_else(|| {
            Refusal::Failed(format!("The encoder {id} is not available on this system."))
        })?;
    let mut only = Registry::new();
    only.register(factory)
        .map_err(|e| Refusal::Failed(e.to_string()))?;
    only.probe_all();
    Ok(only)
}

pub(super) fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    }
}

/// `base`, or `base 2`, `base 3`... whichever no source has yet.
/// `base`, else `base 2`, `base 3`, ...: the first name no scene has.
pub(super) fn unique_scene_name(collection: &SceneCollection, base: &str) -> String {
    let taken = |n: &str| collection.scenes().any(|s| s.name == n);
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|i| format!("{base} {i}"))
        .find(|n| !taken(n))
        .unwrap_or_else(|| base.to_owned())
}

pub(super) fn unique_name(collection: &SceneCollection, base: &str) -> String {
    let base = if base.trim().is_empty() {
        "Source"
    } else {
        base.trim()
    };
    let taken = |n: &str| collection.sources().any(|s| s.name == n);
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|i| format!("{base} {i}"))
        .find(|n| !taken(n))
        .unwrap_or_else(|| base.to_owned())
}

pub(super) fn unique_rename_name(
    collection: &SceneCollection,
    source: SourceId,
    requested: &str,
) -> String {
    let requested = requested.trim();
    let taken = |name: &str| {
        collection
            .sources()
            .any(|candidate| candidate.id != source && candidate.name == name)
    };
    if !taken(requested) {
        return requested.to_owned();
    }
    (1..)
        .map(|n| format!("{requested} ({n})"))
        .find(|name| !taken(name))
        .unwrap_or_else(|| requested.to_owned())
}

pub(super) fn whole_fps(rate: FrameRate) -> i64 {
    let r = rate.per_second();
    i64::from(r.num().div_ceil(r.den().max(1)))
}

/// The camera's rectangle: a 16:9 picture 4/15 of the canvas wide, a margin of 1/40
/// of the width from the bottom-right corner (as `rbs-cli record` places it).
pub fn camera_layout(width: u32, height: u32) -> Layout {
    let w = width as f32 * 4.0 / 15.0;
    let h = w * 9.0 / 16.0;
    let margin = width as f32 / 40.0;
    Layout::new(
        [width as f32 - w - margin, height as f32 - h - margin],
        [w, h],
    )
}

/// The canvas the studio runs (the collection's first, or a new one of `size` at
/// `fps`) and the scene it shows (that canvas's program, else the first scene it may
/// show, else a new shared scene), set on air.
pub(super) fn shown_scene(
    collection: &mut SceneCollection,
    size: Size,
    fps: FrameRate,
) -> Result<(CanvasId, SceneId), Refusal> {
    let first = collection.canvases().next().map(|c| c.id);
    let canvas = match first {
        Some(c) => c,
        None => {
            collection
                .add_canvas("Main", size, fps)
                .map_err(|e| Refusal::Failed(format!("canvas: {e:?}")))?
                .0
        }
    };
    let program = collection.canvas(canvas).and_then(|c| c.program);
    let allowed = collection
        .showable_scenes(canvas)
        .into_iter()
        .next()
        .map(|s| s.id);
    let scene = match program.or(allowed) {
        Some(s) => s,
        None => {
            collection
                .add_scene("Scene", SceneScope::Shared)
                .map_err(|e| Refusal::Failed(format!("scene: {e:?}")))?
                .0
        }
    };
    if program != Some(scene) {
        collection
            .set_program(canvas, Some(scene))
            .map_err(|e| Refusal::Failed(format!("program: {e:?}")))?;
    }
    Ok((canvas, scene))
}

/// A container's file extension.
pub(super) fn container_ext(container: ContainerId) -> &'static str {
    match container {
        ContainerId::Matroska => "mkv",
        ContainerId::Mp4Fragmented => "mp4",
        ContainerId::MpegTs => "ts",
        ContainerId::Flv => "flv",
        ContainerId::Wav => "wav",
        ContainerId::Flac => "flac",
        ContainerId::Mov => "mov",
        _ => "bin",
    }
}

/// Removes what a refused start left in the recording folder it made: the named files,
/// the manifest and its temporary file, then the folder when nothing else is in it.
/// A delete that fails is left for the user (the start already answers `failed`).
/// Removes the files a refused start may have made and its folder. Returns the names
/// still there (a file another program holds, on Windows).
pub(super) fn remove_recording_folder(folder: &Path, names: &[String]) -> Vec<String> {
    const MANIFEST_FILE: &str = "recording.rbs.toml";
    let mut left = Vec::new();
    for name in names
        .iter()
        .map(String::as_str)
        .chain([MANIFEST_FILE, &format!(".{MANIFEST_FILE}.tmp")])
    {
        let path = folder.join(name);
        if std::fs::remove_file(&path).is_err() && path.exists() {
            left.push(name.to_owned());
        }
    }
    let _ = std::fs::remove_dir(folder);
    left
}

/// The wire name of a control API enum value (`limit`, `video`, ...).
pub(super) fn wire_name<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

/// The control API reason of a gate refusal.
pub(super) fn api_refusal(why: rbs_record::estimate::GateRefusal) -> api::RefusalReason {
    use rbs_record::estimate::GateRefusal as G;
    match why {
        G::HardwareLimit => api::RefusalReason::HardwareLimit,
        G::OverLimit => api::RefusalReason::OverLimit,
        G::Heavy => api::RefusalReason::Heavy,
    }
}

/// A start refused by the gate (control API 1.54): `judged` is the plan it judged,
/// `tracks` the files concerned, `software` the video files whose encoder would have
/// fallen to a software one (`software_fallback` reasons).
pub(super) fn gate_refusal(
    reason: api::RefusalReason,
    judged: &rbs_record::estimate::Estimate,
    tracks: &[rbs_record::estimate::TrackRef],
    software: &[SourceId],
) -> StartError {
    let mut estimate = api_estimate(judged, None);
    add_software_fallback(&mut estimate.reasons, software);
    let tracks: Vec<api::EstimateTrack> = tracks
        .iter()
        .map(|t| api::EstimateTrack {
            source: t.source.get(),
            role: api_track_role(t.role),
        })
        .collect();
    let message = match reason {
        api::RefusalReason::HardwareLimit => format!(
            "{} separate video file(s) are more than their encoder can take; start with \
             on_hardware_limit \"skip\" to record without them",
            tracks.len()
        ),
        api::RefusalReason::Heavy => "the recording would be heavy for this computer; start \
             with acknowledge_heavy to record anyway"
            .to_owned(),
        api::RefusalReason::OpenFailed => "nothing was recorded".to_owned(),
        _ => "the separate video files are over a limit of this computer".to_owned(),
    };
    StartError::Refused(
        message,
        Box::new(api::StartRefusal {
            reason,
            estimate,
            tracks,
        }),
    )
}

/// Adds a `software_fallback` reason for each video file of `software`.
pub(super) fn add_software_fallback(reasons: &mut Vec<api::EstimateReason>, software: &[SourceId]) {
    for source in software {
        reasons.push(api::EstimateReason {
            code: api::EstimateCode::SoftwareFallback,
            resource: None,
            track: Some(api::EstimateTrack {
                source: source.get(),
                role: api::TrackRole::Video,
            }),
        });
    }
}

/// The control API's spelling of a `run` value (ADR-0152).
/// A page's deprecated `keep_running`, computed from its activity policy (ADR-0152
/// decision 10): true when its effective `run` is `placed`.
pub(super) fn keeps_running(source: &Source) -> bool {
    rbs_sources::activity_traits(&source.type_id)
        .effective(Some(&source.activity))
        .run
        == rbs_scene::activity::Run::Placed
}

/// A page's settings as the Page tab reads them: the stored keys and the computed
/// `keep_running` (true only), which is no longer stored (ADR-0152 decision 10).
pub(super) fn browser_page_settings(source: &Source) -> Settings {
    let mut settings = source.settings.clone();
    if keeps_running(source) {
        settings.insert(keys::KEEP_RUNNING.to_owned(), SettingValue::Bool(true));
    }
    settings
}

/// A film's deprecated `restart_when_shown`, computed from its activity policy
/// (ADR-0152 decision 10): true unless its effective `on_return` is not `restart`.
pub(super) fn restarts_when_shown(source: &Source) -> bool {
    rbs_sources::activity_traits(&source.type_id)
        .effective(Some(&source.activity))
        .on_return
        .is_none_or(|r| r == rbs_scene::activity::OnReturn::Restart)
}

/// The control API's spelling of a `run` value (ADR-0152; the window sends it too).
pub fn api_run(r: rbs_scene::activity::Run) -> api::ActivityRun {
    use rbs_scene::activity::Run;
    match r {
        Run::Placed => api::ActivityRun::Placed,
        Run::Shown => api::ActivityRun::Shown,
        _ => api::ActivityRun::OnAir,
    }
}

/// The scene model's `run` value for the control API's spelling (ADR-0152).
pub(super) fn activity_run(r: api::ActivityRun) -> rbs_scene::activity::Run {
    use rbs_scene::activity::Run;
    match r {
        api::ActivityRun::Placed => Run::Placed,
        api::ActivityRun::Shown => Run::Shown,
        api::ActivityRun::OnAir => Run::OnAir,
    }
}

/// The control API's spelling of an `on_return` value (ADR-0152).
pub fn api_return(o: rbs_scene::activity::OnReturn) -> api::ActivityReturn {
    use rbs_scene::activity::OnReturn;
    match o {
        OnReturn::Continue => api::ActivityReturn::Continue,
        OnReturn::Resume => api::ActivityReturn::Resume,
        _ => api::ActivityReturn::Restart,
    }
}

/// The scene model's `on_return` value for the control API's spelling (ADR-0152).
pub(super) fn activity_return(o: api::ActivityReturn) -> rbs_scene::activity::OnReturn {
    use rbs_scene::activity::OnReturn;
    match o {
        api::ActivityReturn::Continue => OnReturn::Continue,
        api::ActivityReturn::Resume => OnReturn::Resume,
        api::ActivityReturn::Restart => OnReturn::Restart,
    }
}
