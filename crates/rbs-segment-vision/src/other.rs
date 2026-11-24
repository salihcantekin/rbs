//! The backend on platforms without Apple Vision: always unavailable.

use rbs_segment::{
    BackendInfo, PersonMaskBackend, PersonMaskSession, SegmentError, SegmentHost, SessionRequest,
};

/// Apple Vision is not here: `probe` says unavailable and `open` refuses.
#[derive(Debug, Default, Clone, Copy)]
pub struct VisionBackend;

impl VisionBackend {
    /// Creates the backend.
    pub const fn new() -> Self {
        Self
    }
}

impl PersonMaskBackend for VisionBackend {
    fn type_id(&self) -> &'static str {
        crate::TYPE_ID
    }

    fn probe(&self, _host: &dyn SegmentHost) -> BackendInfo {
        BackendInfo::unavailable()
    }

    fn open(&self, _request: SessionRequest) -> Result<Box<dyn PersonMaskSession>, SegmentError> {
        Err(SegmentError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoModels;
    impl SegmentHost for NoModels {
        fn model(&self, _name: &str) -> Option<rbs_segment::ModelRef> {
            None
        }
    }

    #[test]
    fn it_is_unavailable_off_macos() {
        let backend = VisionBackend::new();
        assert!(!backend.probe(&NoModels).available);
        let request =
            SessionRequest::new(rbs_segment::LevelId::Low, rbs_core::media::Size::new(8, 8));
        assert!(matches!(
            backend.open(request),
            Err(SegmentError::Unavailable)
        ));
        assert_eq!(backend.type_id(), crate::TYPE_ID);
    }
}
