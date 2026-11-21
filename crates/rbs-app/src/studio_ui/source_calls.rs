//! Source operations of the window, sent as control API commands (ADR-0163 step 3):
//! rename, the per-source slide, keep the device open, the sync offset, Duplicate
//! source, Copy attributes and the "Capture audio too" companion.

use rbs_api::Command as C;
use rbs_api::request as rq;
use rbs_api::types::{Glide, GlideEasing, OutputCanvas};

use super::StudioUi;

/// Source `id` in the scene being edited, as the attribute and duplicate calls
/// name it.
fn in_scene(id: i32) -> rq::SourceInScene {
    rq::SourceInScene {
        source: id as u32,
        scene: None,
        canvas: OutputCanvas::default(),
    }
}

impl StudioUi {
    pub fn rename_source(&mut self, id: i32, name: &str) {
        self.deleted.clear();
        self.call(C::SourceRename(rq::RenameSource {
            source: id as u32,
            name: name.to_owned(),
        }));
    }

    /// Per-source slide override (ADR-0100). `custom` false clears it.
    pub fn set_source_glide(&mut self, id: i32, custom: bool, duration_ms: i32, easing: i32) {
        let glide = custom.then(|| Glide {
            duration_ms: duration_ms.clamp(0, 5_000) as u32,
            easing: if easing == 0 {
                GlideEasing::Linear
            } else {
                GlideEasing::EaseInOut
            },
        });
        self.call(C::SourceSetGlide(rq::SetSourceGlide {
            source: id as u32,
            glide,
        }));
    }

    /// Keeps a source's device open, or releases it. Hiding does not call this.
    pub fn set_source_active(&mut self, id: i32, active: bool) {
        self.deleted.clear();
        self.call(C::SourceSetActive(rq::SetSourceActive {
            source: id as u32,
            active,
        }));
    }

    pub fn set_source_sync_offset(&mut self, id: i32, ms: i32) {
        self.call(C::SourceSetSyncOffset(rq::SetSourceSyncOffset {
            source: id as u32,
            offset_ms: ms.clamp(-200, 200),
        }));
    }

    /// Duplicate source (ADR-0133 decision 3).
    pub fn duplicate_source(&mut self, id: i32) {
        self.call(C::SourceDuplicate(in_scene(id)));
    }

    /// Copy attributes of a source (ADR-0133 decision 6).
    pub fn copy_attributes(&mut self, id: i32) {
        self.call(C::SourceCopyAttributes(in_scene(id)));
    }

    /// Adds or removes the "Capture audio too" companion of a display or window.
    pub fn set_companion_audio(&mut self, id: i32, on: bool) {
        self.call(C::SourceSetCompanionAudio(rq::SetCompanionAudio {
            source: id as u32,
            enabled: on,
        }));
    }
}
