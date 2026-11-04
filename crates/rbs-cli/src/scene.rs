//! The scene `record` captures, built in code until scene documents can be loaded:
//! a display filling the canvas, a camera in the bottom-right corner, the default
//! microphone and the system audio (each left out when absent or not wanted).

use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use std::collections::BTreeMap;

use rbs_capture::CaptureError;
use rbs_scene::SceneCollection;
use rbs_scene::model::{
    CanvasId, ItemContent, Layout, SceneScope, SettingValue, Settings, SourceId,
};
use rbs_sources::types::{self, keys};
use rbs_sources::{SourceError, SourceTarget, SourceTypes};

use crate::args::RecordArgs;
use crate::permissions::Check;

/// The requested camera picture (most webcams and capture cards offer it).
const CAMERA_SIZE: (i64, i64, i64) = (1280, 720, 30);

/// A collection with one canvas and one scene on air.
#[derive(Debug)]
pub struct BuiltScene {
    /// The collection.
    pub collection: SceneCollection,
    /// Its canvas.
    pub canvas: CanvasId,
    /// Sources left out, and why.
    pub skipped: Vec<String>,
    /// Sync offsets in ns of the audio sources that have one (positive delays).
    pub audio_offsets: BTreeMap<SourceId, i64>,
}

fn targets(types: &SourceTypes, type_id: &'static str) -> Vec<SourceTarget> {
    types.targets(&rbs_graph_type(type_id)).unwrap_or_default()
}

fn rbs_graph_type(type_id: &'static str) -> NodeTypeId {
    NodeTypeId::new_static(type_id)
}

fn pick<'a>(
    list: &'a [SourceTarget],
    index: Option<usize>,
    what: &str,
) -> Result<Option<&'a SourceTarget>, String> {
    match index {
        None => Ok(list.iter().find(|t| t.is_default).or_else(|| list.first())),
        Some(i) => list.get(i).map(Some).ok_or_else(|| {
            format!(
                "there is no {what} {i}; {} found (see `rbs-cli devices`)",
                list.len()
            )
        }),
    }
}

fn target_settings(t: &SourceTarget) -> Settings {
    let mut s = Settings::new();
    s.insert(
        keys::ADAPTER.into(),
        SettingValue::Text(t.adapter.as_str().to_owned()),
    );
    s.insert(keys::TARGET.into(), SettingValue::Text(t.target.clone()));
    s
}

/// Whether `permit` refuses `target`; the refusal is noted in `skipped`.
fn refused(
    permit: &Check<'_>,
    type_id: &'static str,
    target: &SourceTarget,
    name: &str,
    skipped: &mut Vec<String>,
) -> bool {
    match permit(type_id, &target.adapter) {
        Some(why) => {
            skipped.push(format!("{name}: {why}"));
            true
        }
        None => false,
    }
}

fn whole_fps(rate: FrameRate) -> i64 {
    let r = rate.per_second();
    i64::from(r.num().div_ceil(r.den().max(1)))
}

/// A display from the OS picker, for systems that list none (the Linux screen-cast
/// portal). Blocks while the dialog is up.
fn choose_display(types: &SourceTypes) -> Result<SourceTarget, String> {
    let picker = types
        .pickers(&rbs_graph_type(types::DISPLAY_SOURCE))
        .unwrap_or_default()
        .into_iter()
        .next()
        .ok_or("no display can be captured on this system")?;
    println!("choose the display to record in the system dialog");
    picker.pick().map_err(|e| match e {
        SourceError::Capture(CaptureError::Cancelled) => {
            "the display dialog was closed without a choice".to_owned()
        }
        e => format!("display: {e}"),
    })
}

/// The camera's rectangle: a 16:9 picture 4/15 of the canvas wide, a margin of 1/40
/// of the width from the bottom-right corner. On 1920 x 1080 it is 512 x 288 with its
/// top-left corner at (1360, 744).
pub fn camera_layout(width: u32, height: u32) -> Layout {
    let w = width as f32 * 4.0 / 15.0;
    let h = w * 9.0 / 16.0;
    let margin = width as f32 / 40.0;
    Layout::new(
        [width as f32 - w - margin, height as f32 - h - margin],
        [w, h],
    )
}

/// Builds the scene from what `types` (probed) can capture. `permit` says why a
/// picked camera or audio endpoint may not be captured (it may ask the OS); such a
/// source is left out and said so.
pub fn build(
    types: &SourceTypes,
    args: &RecordArgs,
    permit: &Check<'_>,
) -> Result<BuiltScene, String> {
    let (w, h) = (args.size.width, args.size.height);
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", args.size, args.fps)
        .map_err(|e| format!("canvas: {e:?}"))?;
    let (scene, _) = col
        .add_scene("Recording", SceneScope::Shared)
        .map_err(|e| format!("scene: {e:?}"))?;
    let mut skipped = Vec::new();
    let add = |col: &mut SceneCollection,
               name: &str,
               type_id: &'static str,
               settings: Settings,
               layout: Layout|
     -> Result<SourceId, String> {
        let (id, _) = col
            .add_source(name, rbs_graph_type(type_id), settings)
            .map_err(|e| format!("{name}: {e:?}"))?;
        col.add_item(scene, ItemContent::Source(id), layout, None)
            .map_err(|e| format!("{name}: {e:?}"))?;
        Ok(id)
    };
    let mut audio_offsets = BTreeMap::new();

    let displays = targets(types, types::DISPLAY_SOURCE);
    let chosen;
    let display = match pick(&displays, args.display, "display")? {
        Some(d) => d,
        None => {
            chosen = choose_display(types)?;
            &chosen
        }
    };
    let mut settings = target_settings(display);
    settings.insert(keys::FPS.into(), SettingValue::Int(whole_fps(args.fps)));
    add(
        &mut col,
        "Display",
        types::DISPLAY_SOURCE,
        settings,
        // Its own aspect ratio, centred: a 21:9 display is not squeezed into 16:9.
        display
            .size
            .map_or(Layout::new([0.0, 0.0], [w as f32, h as f32]), |(sw, sh)| {
                Layout::fit(w, h, sw, sh)
            }),
    )?;

    if args.no_camera {
        skipped.push("camera: --no-camera".to_owned());
    } else {
        let cameras = targets(types, types::CAMERA_SOURCE);
        match pick(&cameras, args.camera, "camera")? {
            Some(cam) if refused(permit, types::CAMERA_SOURCE, cam, "camera", &mut skipped) => {}
            Some(cam) => {
                let mut settings = target_settings(cam);
                let (cw, ch, cfps) = CAMERA_SIZE;
                settings.insert(keys::WIDTH.into(), SettingValue::Int(cw));
                settings.insert(keys::HEIGHT.into(), SettingValue::Int(ch));
                settings.insert(keys::FPS.into(), SettingValue::Int(cfps));
                add(
                    &mut col,
                    "Camera",
                    types::CAMERA_SOURCE,
                    settings,
                    camera_layout(w, h),
                )?;
            }
            None => skipped.push("camera: none found".to_owned()),
        }
    }

    for (name, type_id, off, why, offset_ms) in [
        (
            "Microphone",
            types::AUDIO_INPUT_SOURCE,
            args.no_mic || args.no_audio,
            "--no-mic",
            args.mic_offset_ms,
        ),
        (
            "System audio",
            types::AUDIO_OUTPUT_SOURCE,
            args.no_system_audio || args.no_audio,
            "--no-system-audio",
            args.system_audio_offset_ms,
        ),
    ] {
        if off {
            skipped.push(format!("{name}: {why}"));
            continue;
        }
        let list = targets(types, type_id);
        match pick(&list, None, name)? {
            Some(t) if refused(permit, type_id, t, name, &mut skipped) => {}
            Some(t) => {
                let id = add(
                    &mut col,
                    name,
                    type_id,
                    target_settings(t),
                    Layout::new([0.0, 0.0], [1.0, 1.0]),
                )?;
                if offset_ms != 0 {
                    audio_offsets.insert(id, i64::from(offset_ms) * 1_000_000);
                }
            }
            None => skipped.push(format!("{name}: none found")),
        }
    }
    col.set_program(canvas, Some(scene))
        .map_err(|e| format!("program: {e:?}"))?;
    Ok(BuiltScene {
        collection: col,
        canvas,
        skipped,
        audio_offsets,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::Arc;

    use rbs_audio_io_test::TestAudioBackend;
    use rbs_capture_test::TestCaptureFactory;
    use rbs_core::registry::AdapterId;

    use super::*;
    use crate::args::{Command, parse};

    fn args(extra: &[&str]) -> RecordArgs {
        let all = ["record", "x.mkv"].iter().chain(extra).copied();
        match parse(all).unwrap() {
            Command::Record(a) => *a,
            other => panic!("{other:?}"),
        }
    }

    fn types() -> SourceTypes {
        let mut t = SourceTypes::new();
        t.register_capture(Arc::new(TestCaptureFactory::new()))
            .unwrap();
        t.register_audio(Arc::new(TestAudioBackend::new())).unwrap();
        t.probe();
        t
    }

    fn allow(_: &'static str, _: &AdapterId) -> Option<String> {
        None
    }

    fn names(b: &BuiltScene) -> Vec<String> {
        b.collection.sources().map(|s| s.name.clone()).collect()
    }

    #[test]
    fn the_scene_has_display_camera_microphone_and_system_audio() {
        let b = build(&types(), &args(&[]), &allow).unwrap();
        let mut n = names(&b);
        n.sort();
        assert_eq!(n, ["Camera", "Display", "Microphone", "System audio"]);
        assert!(b.skipped.is_empty());
        assert!(b.audio_offsets.is_empty());
        let cam = b.collection.sources().find(|s| s.name == "Camera").unwrap();
        assert_eq!(cam.settings[keys::WIDTH], SettingValue::Int(1280));
    }

    #[test]
    fn options_leave_sources_out_and_bad_indices_are_refused() {
        let b = build(&types(), &args(&["--no-camera", "--no-audio"]), &allow).unwrap();
        assert_eq!(names(&b), ["Display"]);
        assert_eq!(b.skipped.len(), 3);
        let err = build(&types(), &args(&["--display", "99"]), &allow).unwrap_err();
        assert!(err.contains("no display 99"), "{err}");
        let empty = SourceTypes::new();
        assert!(build(&empty, &args(&[]), &allow).is_err());
    }

    #[test]
    fn audio_offsets_are_kept_per_source_in_nanoseconds() {
        let b = build(
            &types(),
            &args(&["--mic-offset", "-30", "--system-audio-offset", "25"]),
            &allow,
        )
        .unwrap();
        let id = |name: &str| b.collection.sources().find(|s| s.name == name).unwrap().id;
        let want = BTreeMap::from([
            (id("Microphone"), -30_000_000),
            (id("System audio"), 25_000_000),
        ]);
        assert_eq!(b.audio_offsets, want);
        let b = build(&types(), &args(&["--no-mic", "--mic-offset", "10"]), &allow).unwrap();
        assert!(
            b.audio_offsets.is_empty(),
            "no offset for a source left out"
        );
    }

    #[test]
    fn a_camera_or_microphone_without_permission_is_left_out_and_said_so() {
        let refuse = |t: &'static str, _: &AdapterId| {
            (t == types::CAMERA_SOURCE || t == types::AUDIO_INPUT_SOURCE)
                .then(|| "permission not granted".to_owned())
        };
        let b = build(&types(), &args(&[]), &refuse).unwrap();
        let mut n = names(&b);
        n.sort();
        assert_eq!(n, ["Display", "System audio"]);
        assert_eq!(
            b.skipped,
            [
                "camera: permission not granted",
                "Microphone: permission not granted"
            ]
        );
    }

    #[test]
    fn the_camera_sits_in_the_bottom_right_corner() {
        let l = camera_layout(1920, 1080);
        assert_eq!(l, Layout::new([1360.0, 744.0], [512.0, 288.0]));
    }
}
