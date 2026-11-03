//! The scene `record` captures, built in code until scene documents can be loaded:
//! a display filling the canvas, a camera in the bottom-right corner, the default
//! microphone and the system audio (each left out when absent or not wanted).

use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::SceneCollection;
use rbs_scene::model::{CanvasId, ItemContent, Layout, SceneScope, SettingValue, Settings};
use rbs_sources::types::{self, keys};
use rbs_sources::{SourceTarget, SourceTypes};

use crate::args::RecordArgs;

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

fn whole_fps(rate: FrameRate) -> i64 {
    let r = rate.per_second();
    i64::from(r.num().div_ceil(r.den().max(1)))
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

/// Builds the scene from what `types` (probed) can capture.
pub fn build(types: &SourceTypes, args: &RecordArgs) -> Result<BuiltScene, String> {
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
     -> Result<(), String> {
        let (id, _) = col
            .add_source(name, rbs_graph_type(type_id), settings)
            .map_err(|e| format!("{name}: {e:?}"))?;
        col.add_item(scene, ItemContent::Source(id), layout, None)
            .map_err(|e| format!("{name}: {e:?}"))?;
        Ok(())
    };

    let displays = targets(types, types::DISPLAY_SOURCE);
    let display = pick(&displays, args.display, "display")?
        .ok_or("no display can be captured on this system")?;
    let mut settings = target_settings(display);
    settings.insert(keys::FPS.into(), SettingValue::Int(whole_fps(args.fps)));
    add(
        &mut col,
        "Display",
        types::DISPLAY_SOURCE,
        settings,
        Layout::new([0.0, 0.0], [w as f32, h as f32]),
    )?;

    if args.no_camera {
        skipped.push("camera: --no-camera".to_owned());
    } else {
        let cameras = targets(types, types::CAMERA_SOURCE);
        match pick(&cameras, args.camera, "camera")? {
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

    for (name, type_id, off, why) in [
        (
            "Microphone",
            types::AUDIO_INPUT_SOURCE,
            args.no_mic || args.no_audio,
            "--no-mic",
        ),
        (
            "System audio",
            types::AUDIO_OUTPUT_SOURCE,
            args.no_system_audio || args.no_audio,
            "--no-system-audio",
        ),
    ] {
        if off {
            skipped.push(format!("{name}: {why}"));
            continue;
        }
        let list = targets(types, type_id);
        match pick(&list, None, name)? {
            Some(t) => add(
                &mut col,
                name,
                type_id,
                target_settings(t),
                Layout::new([0.0, 0.0], [1.0, 1.0]),
            )?,
            None => skipped.push(format!("{name}: none found")),
        }
    }
    col.set_program(canvas, Some(scene))
        .map_err(|e| format!("program: {e:?}"))?;
    Ok(BuiltScene {
        collection: col,
        canvas,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::Arc;

    use rbs_audio_io_test::TestAudioBackend;
    use rbs_capture_test::TestCaptureFactory;

    use super::*;
    use crate::args::{Command, parse};

    fn args(extra: &[&str]) -> RecordArgs {
        let all = ["record", "x.mkv"].iter().chain(extra).copied();
        match parse(all).unwrap() {
            Command::Record(a) => a,
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

    fn names(b: &BuiltScene) -> Vec<String> {
        b.collection.sources().map(|s| s.name.clone()).collect()
    }

    #[test]
    fn the_scene_has_display_camera_microphone_and_system_audio() {
        let b = build(&types(), &args(&[])).unwrap();
        let mut n = names(&b);
        n.sort();
        assert_eq!(n, ["Camera", "Display", "Microphone", "System audio"]);
        assert!(b.skipped.is_empty());
        let cam = b.collection.sources().find(|s| s.name == "Camera").unwrap();
        assert_eq!(cam.settings[keys::WIDTH], SettingValue::Int(1280));
    }

    #[test]
    fn options_leave_sources_out_and_bad_indices_are_refused() {
        let b = build(&types(), &args(&["--no-camera", "--no-audio"])).unwrap();
        assert_eq!(names(&b), ["Display"]);
        assert_eq!(b.skipped.len(), 3);
        let err = build(&types(), &args(&["--display", "99"])).unwrap_err();
        assert!(err.contains("no display 99"), "{err}");
        let empty = SourceTypes::new();
        assert!(build(&empty, &args(&[])).is_err());
    }

    #[test]
    fn the_camera_sits_in_the_bottom_right_corner() {
        let l = camera_layout(1920, 1080);
        assert_eq!(l, Layout::new([1360.0, 744.0], [512.0, 288.0]));
    }
}
