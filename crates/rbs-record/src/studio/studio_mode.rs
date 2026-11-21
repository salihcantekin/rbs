//! Studio mode (ADR-0061): the preview scene's compositor and picture.

use rbs_gpu::share::PreviewEndpoint;

use super::Studio;

impl Studio {
    /// Where a UI connects to the preview scene's picture (studio mode, ADR-0061), once
    /// it runs. `None` outside studio mode and without a preview description.
    pub fn scene_preview(&self) -> Option<PreviewEndpoint> {
        self.scene_preview
            .as_ref()
            .and_then(|sp| sp.picture.1.endpoint())
    }

    /// True while the preview scene's compositor and picture run (studio mode with a
    /// preview description, ADR-0061).
    pub fn studio_mode(&self) -> bool {
        self.scene_preview.is_some()
    }
}
