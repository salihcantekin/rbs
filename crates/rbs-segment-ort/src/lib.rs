//! ONNX Runtime person segmentation as a backend of the person mask port
//! (`rbs-segment`; ADR-0179 decisions 3, 8 and 13, `docs/specs/person-mask-port.md`).
//!
//! The backend runs one of three networks per quality level through ONNX Runtime, on the
//! GPU through DirectML on Windows and on the processor elsewhere (a way to test; Linux is
//! not a supported target of the first versions). Everything is optional data and loaded
//! on use (principle P13): the ONNX Runtime library and the model files are found by the
//! composition root, nothing is loaded by `probe`, and no thread exists before `warm`.
//!
//! | Level | Network | Input | Why |
//! | --- | --- | --- | --- |
//! | Low | MediaPipe selfie segmentation | 256x256 | about 1.8 ms on an RTX 4090 |
//! | Medium | MODNet (webcam) | the small picture, sides rounded to 32 | about 6.3 ms |
//! | High | Robust Video Matting, MobileNetV3 | the small picture | about 11 ms, best edges |
//!
//! Models are data, not code: a [`ModelSource`] lends their checked bytes, so a path from
//! a scene document is never a model (spec rule 10).

mod net;
mod session;

use std::path::PathBuf;
use std::sync::Arc;

use rbs_core::media::MemoryKind;
use rbs_segment::{
    BackendInfo, ComputeUnit, CostHint, LevelId, PersonMaskBackend, PersonMaskSession,
    QualityLevel, SegmentError, SegmentHost, SessionRequest,
};

pub use net::{Net, spec_of};

/// The type id of this backend in the registry (ADR-0162).
pub const TYPE_ID: &str = "org.rbs.segment.onnx";

/// Where the models come from. The composition root implements it over its model folder
/// and checks size and hash before `load` returns bytes.
pub trait ModelSource: Send + Sync {
    /// Whether a model of this name is there. Cheap: a file check, no read.
    fn exists(&self, name: &str) -> bool;
    /// The checked bytes of the model, or `None` when it is missing or fails its check.
    fn load(&self, name: &str) -> Option<Arc<[u8]>>;
}

/// The ONNX Runtime backend.
pub struct OrtBackend {
    models: Arc<dyn ModelSource>,
    runtime: PathBuf,
}

impl OrtBackend {
    /// Creates the backend: `runtime` is the ONNX Runtime library file (`onnxruntime.dll`
    /// with DirectML on Windows), `models` lends the networks.
    pub fn new(runtime: impl Into<PathBuf>, models: Arc<dyn ModelSource>) -> Self {
        Self {
            models,
            runtime: runtime.into(),
        }
    }
}

impl PersonMaskBackend for OrtBackend {
    fn type_id(&self) -> &'static str {
        TYPE_ID
    }

    fn probe(&self, _host: &dyn SegmentHost) -> BackendInfo {
        if !self.runtime.is_file() {
            return BackendInfo::unavailable();
        }
        let levels: Vec<QualityLevel> = [LevelId::Low, LevelId::Medium, LevelId::High]
            .into_iter()
            .filter_map(spec_of)
            .filter(|spec| self.models.exists(spec.model))
            .map(|spec| {
                QualityLevel::new(
                    spec.level,
                    CostHint::new(spec.gpu_ms, ComputeUnit::Gpu, spec.note),
                )
            })
            .collect();
        if levels.is_empty() {
            return BackendInfo::unavailable();
        }
        let mut info = BackendInfo::unavailable();
        info.available = true;
        info.temporal = levels.iter().any(|l| l.id == LevelId::High);
        info.levels = levels;
        info.input = MemoryKind::System;
        info.output = MemoryKind::System;
        info.hardware_assisted = cfg!(windows);
        info
    }

    fn open(&self, request: SessionRequest) -> Result<Box<dyn PersonMaskSession>, SegmentError> {
        let spec = spec_of(request.level).ok_or(SegmentError::Unsupported {
            level: request.level,
        })?;
        let bytes = self
            .models
            .load(spec.model)
            .ok_or(SegmentError::Unsupported {
                level: request.level,
            })?;
        Ok(Box::new(session::OrtSession::new(
            spec,
            bytes,
            self.runtime.clone(),
            request.input_size,
        )))
    }
}
