//! Source operations of the window, sent as control API commands (ADR-0163 step 3):
//! rename, the per-source slide, keep the device open, the sync offset, Duplicate
//! source, Copy attributes, Delete source, the "Capture audio too" companion, global sources,
//! the capture region and display chrome.

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

    /// Reload on the Source tab: a source that is not running is tried again.
    pub fn reload_source(&mut self, id: i32) {
        self.call(C::SourceReload(rq::ReloadSource { source: id as u32 }));
    }

    /// Delete source: from every scene, with the undo toast.
    pub fn delete_source(&mut self, id: i32) {
        self.deleted.clear();
        self.call(C::SourceDelete(rq::SourceRef { source: id as u32 }));
    }

    /// Adds or removes the "Capture audio too" companion of a display or window.
    pub fn set_companion_audio(&mut self, id: i32, on: bool) {
        self.call(C::SourceSetCompanionAudio(rq::SetCompanionAudio {
            source: id as u32,
            enabled: on,
        }));
    }

    /// Show in every scene (Properties' Toggle, ADR-0065); off moves the source into
    /// the scene being edited.
    pub fn set_global(&mut self, id: i32, global: bool) {
        self.deleted.clear();
        let source = id as u32;
        self.call(if global {
            C::GlobalAdd(rq::AddGlobal { source })
        } else {
            C::GlobalRemove(rq::RemoveGlobal { source, keep: true })
        });
    }

    /// Settings › Global sources' Remove.
    pub fn remove_global(&mut self, id: i32) {
        self.deleted.clear();
        self.call(C::GlobalRemove(rq::RemoveGlobal {
            source: id as u32,
            keep: false,
        }));
    }

    /// Capture region on the source (ADR-0103). `clear` removes the region.
    pub fn set_source_capture_region(
        &mut self,
        id: i32,
        clear: bool,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    ) {
        if self.host.is_none() {
            return;
        }
        let edges = [left, top, right, bottom].map(|v| v.max(0) as u32);
        self.call(C::SourceSetRegion(rq::SetSourceRegion {
            source: id as u32,
            region: (!clear).then_some(edges),
        }));
    }

    /// Display chrome (ADR-0104). `on` false turns chrome off; `fill` 1 is Fit.
    #[allow(clippy::too_many_arguments)] // One argument per field of the chrome form.
    pub fn set_display_chrome(
        &mut self,
        id: i32,
        on: bool,
        top: i32,
        right: i32,
        bottom: i32,
        left: i32,
        fill: i32,
    ) {
        if self.host.is_none() {
            return;
        }
        let chrome = on.then(|| rq::ChromeSpec {
            top: top.max(0) as u32,
            right: right.max(0) as u32,
            bottom: bottom.max(0) as u32,
            left: left.max(0) as u32,
            fill: if fill == 1 {
                rq::ChromeFill::Fit
            } else {
                rq::ChromeFill::Cover
            },
        });
        self.call(C::SourceSetChrome(rq::SetSourceChrome {
            source: id as u32,
            chrome,
        }));
    }

    pub fn fit_display_chrome(&mut self, id: i32) {
        if self.host.is_some() {
            self.call(C::SourceFitChrome(rq::SourceRef { source: id as u32 }));
        }
    }

    pub fn reset_display_chrome(&mut self, id: i32) {
        if self.host.is_some() {
            self.call(C::SourceResetChrome(rq::SourceRef { source: id as u32 }));
        }
    }
}

impl StudioUi {
    /// The fill of a colour source. A colour that is not `#rrggbb` or `#rrggbbaa`
    /// changes nothing.
    pub fn set_source_fill(&mut self, id: i32, color: &str) {
        let Some(color) = rbs_api::types::parse_color(color.trim()) else {
            return;
        };
        self.deleted.clear();
        let color = serde_json::Value::String(rbs_api::types::format_color(color));
        let key = rbs_sources::types::keys::COLOR.to_owned();
        self.call(rbs_api::Command::SourceSetSettings(
            rbs_api::request::SetSourceSettings {
                source: id as u32,
                settings: std::iter::once((key, color)).collect(),
                gesture: 0,
            },
        ));
    }
}
