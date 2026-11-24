//! The backend against the real ONNX Runtime and the real networks. Needs files the
//! repository does not hold (the ONNX Runtime library and the models, which are data,
//! ADR-0179 decision 8), so these tests are ignored by default:
//!
//! `RBS_ORT_LIB=<libonnxruntime> RBS_ORT_MODELS=<dir with <name>.onnx files>
//!  [RBS_ORT_FRAME=<raw BGRA file> RBS_ORT_FRAME_SIZE=512x288]
//!  cargo test -p rbs-segment-ort -- --ignored`

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use rbs_core::media::{
    Bytes, ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_segment::{
    LevelId, MattePlane, ModelRef, PersonMaskBackend, SegmentHost, SessionRequest, SubmitOutcome,
};
use rbs_segment_ort::{ModelSource, OrtBackend, TYPE_ID};

struct NoModels;
impl SegmentHost for NoModels {
    fn model(&self, _name: &str) -> Option<ModelRef> {
        None
    }
}

struct Folder(PathBuf);
impl ModelSource for Folder {
    fn exists(&self, name: &str) -> bool {
        self.0.join(format!("{name}.onnx")).is_file()
    }
    fn load(&self, name: &str) -> Option<Arc<[u8]>> {
        std::fs::read(self.0.join(format!("{name}.onnx")))
            .ok()
            .map(Arc::from)
    }
}

struct Missing;
impl ModelSource for Missing {
    fn exists(&self, _name: &str) -> bool {
        false
    }
    fn load(&self, _name: &str) -> Option<Arc<[u8]>> {
        None
    }
}

fn frame(size: Size, data: Vec<u8>, n: u64) -> VideoFrame {
    let format = VideoFormat::new(size, PixelFormat::Bgra8, ColorSpace::SRGB);
    let system = SystemFrame::new(
        format,
        Bytes::from(data),
        &[PlaneLayout {
            offset: 0,
            stride: size.width as usize * 4,
        }],
    )
    .unwrap();
    VideoFrame::system(Timestamp::from_nanos(n * 1_000_000), system, None)
}

#[test]
fn the_type_id_is_stable() {
    let backend = OrtBackend::new("nowhere/onnxruntime", Arc::new(Missing));
    assert_eq!(backend.type_id(), TYPE_ID);
    assert_eq!(TYPE_ID, "org.rbs.segment.onnx");
}

#[test]
fn it_is_unavailable_without_the_runtime_or_models() {
    let backend = OrtBackend::new("nowhere/onnxruntime", Arc::new(Missing));
    assert!(!backend.probe(&NoModels).available);
    let here = std::env::current_exe().unwrap();
    let backend = OrtBackend::new(here, Arc::new(Missing));
    assert!(!backend.probe(&NoModels).available, "no models, no levels");
}

#[test]
#[ignore = "slow: needs ONNX Runtime and the models; see the file header"]
fn slow_every_level_cuts_a_person_out_of_a_real_picture() {
    let (Ok(lib), Ok(models)) = (
        std::env::var("RBS_ORT_LIB"),
        std::env::var("RBS_ORT_MODELS"),
    ) else {
        panic!("set RBS_ORT_LIB and RBS_ORT_MODELS");
    };
    let raw = std::fs::read(std::env::var("RBS_ORT_FRAME").expect("RBS_ORT_FRAME")).unwrap();
    let dims = std::env::var("RBS_ORT_FRAME_SIZE").expect("RBS_ORT_FRAME_SIZE");
    let (w, h) = dims.split_once('x').unwrap();
    let size = Size::new(w.parse().unwrap(), h.parse().unwrap());
    let backend = OrtBackend::new(lib, Arc::new(Folder(PathBuf::from(models))));
    let info = backend.probe(&NoModels);
    assert!(info.available && info.levels.len() == 3, "{info:?}");
    for level in [LevelId::Low, LevelId::Medium, LevelId::High] {
        let mut session = backend.open(SessionRequest::new(level, size)).unwrap();
        session.warm().unwrap();
        let mut covered = 0.0f32;
        // A few pictures: RVM carries state, the first picture is the roughest.
        for n in 0..4 {
            assert_eq!(
                session.submit(&frame(size, raw.clone(), n)),
                SubmitOutcome::Accepted
            );
            let matte = loop {
                if let Some(m) = session.latest().unwrap() {
                    break m;
                }
                std::thread::yield_now();
            };
            let MattePlane::Cpu(pixels) = &matte.plane else {
                panic!("a processor matte");
            };
            assert_eq!(
                pixels.len(),
                (matte.size.width * matte.size.height) as usize
            );
            covered = pixels.iter().filter(|p| **p > 127).count() as f32 / pixels.len() as f32;
        }
        println!(
            "{level:?}: {:.1} percent of the picture is person",
            covered * 100.0
        );
        assert!(
            (0.05..0.7).contains(&covered),
            "{level:?} covered {covered}: not a person-sized area"
        );
        session.close();
    }
}
