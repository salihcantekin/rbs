//! Scene and transition operations of the window, sent as control API commands
//! (ADR-0163 step 1): the Scenes panel, its row menu and the studio mode controls.

use rbs_api::Command as C;
use rbs_api::request as rq;
use rbs_api::types as api;
use rbs_app::studio_host::ApiReply;

use super::StudioUi;

impl StudioUi {
    pub fn add_scene(&mut self) {
        self.deleted.clear();
        self.call(C::SceneAdd(rq::AddScene { name: None }));
    }

    /// Copies a scene; it does not go on air.
    pub fn duplicate_scene(&mut self, id: i32) {
        self.duplicate(id, api::DuplicateSources::Shared);
    }

    /// Duplicate scene with new sources (ADR-0133 decision 4).
    pub fn duplicate_scene_with_copies(&mut self, id: i32) {
        self.duplicate(id, api::DuplicateSources::Copied);
    }

    fn duplicate(&mut self, id: i32, sources: api::DuplicateSources) {
        self.deleted.clear();
        self.call(C::SceneDuplicate(rq::DuplicateScene {
            scene: id as u32,
            sources,
        }));
    }

    pub fn remove_scene(&mut self, id: i32) {
        self.deleted.clear();
        self.call(C::SceneRemove(rq::SceneRef { scene: id as u32 }));
    }

    pub fn rename_scene(&mut self, id: i32, name: &str) {
        self.deleted.clear();
        self.call(C::SceneRename(rq::RenameScene {
            scene: id as u32,
            name: name.to_owned(),
        }));
    }

    pub fn move_scene(&mut self, id: i32, direction: rq::LayerDirection) {
        self.deleted.clear();
        self.call(C::SceneMove(rq::MoveScene {
            scene: id as u32,
            direction,
        }));
    }

    /// A scene's transition into it (ADR-0135): `kind` -1 the collection's, else the
    /// rail's index; `duration_ms` -1 the kind's own duration.
    pub fn set_scene_transition(&mut self, id: i32, kind: i32, duration_ms: i32) {
        let kind = match kind {
            0 => Some(api::TransitionKind::Cut),
            1 => Some(api::TransitionKind::Fade),
            2 => Some(api::TransitionKind::Move),
            3 => Some(api::TransitionKind::Swipe),
            4 => Some(api::TransitionKind::Slide),
            5 => Some(api::TransitionKind::Stinger),
            _ => None,
        };
        let duration_ms = u32::try_from(duration_ms).ok().map(|ms| ms.min(5_000));
        let transition = (kind.is_some() || duration_ms.is_some())
            .then_some(api::SceneTransition { kind, duration_ms });
        self.call(C::SceneSetTransition(rq::SetSceneTransition {
            scene: id as u32,
            transition,
        }));
    }

    /// Collection glide defaults (ADR-0100).
    pub fn set_glide(&mut self, duration_ms: i32, easing: i32) {
        self.send_glide(duration_ms.clamp(0, 5_000) as u32, easing != 0);
    }

    /// The collection glide's duration and whether it eases in and out.
    pub(super) fn send_glide(&self, duration_ms: u32, ease: bool) {
        self.call(C::CollectionSetGlide(rq::SetCollectionGlide {
            duration_ms,
            easing: if ease {
                api::GlideEasing::EaseInOut
            } else {
                api::GlideEasing::Linear
            },
        }));
    }

    /// The T-bar moved to `t` (ADR-0135 decision 1). Quiet: a step that arrives after
    /// studio mode ended is ignored, as before (ADR-0163 decision 2).
    pub fn transition_progress(&mut self, t: f32) {
        if self.studio_mode {
            self.call_as(
                C::CanvasTransitionProgress(rq::TransitionProgress {
                    canvas: None,
                    t: t.clamp(0.0, 1.0),
                }),
                ApiReply::Quiet,
            );
        }
    }

    /// The transition: the preview scene goes on air (ADR-0061).
    pub fn transition(&mut self) {
        if self.studio_mode {
            self.call(C::CanvasTransition(rq::TransitionOnce::default()));
        }
    }
}
