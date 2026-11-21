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

impl StudioUi {
    /// Sends the parts of `t` that differ from the transition the window last saw as
    /// `collection.set_transition` (ADR-0163): a change made elsewhere meanwhile to
    /// another part is kept.
    pub(super) fn send_transition(&self, t: rbs_scene::CollectionTransition) {
        self.call(C::CollectionSetTransition(transition_patch(
            &self.transition,
            &t,
        )));
    }
}

/// The `collection.set_transition` request that turns `old` into `new`.
fn transition_patch(
    old: &rbs_scene::CollectionTransition,
    new: &rbs_scene::CollectionTransition,
) -> rq::SetCollectionTransition {
    rq::SetCollectionTransition {
        kind: (old.kind != new.kind).then(|| transition_kind(new.kind)),
        direct_kind: (old.direct != new.direct).then(|| transition_kind(new.direct)),
        fade: (old.fade != new.fade).then(|| api::FadeTransition {
            duration_ms: new.fade.duration_ms,
            easing: easing(new.fade.easing),
            via: new
                .fade
                .via
                .map(|[r, g, b]| format!("#{r:02x}{g:02x}{b:02x}")),
        }),
        swipe: (old.swipe != new.swipe).then(|| swipe(new.swipe)),
        slide: (old.slide != new.slide).then(|| swipe(new.slide)),
        stinger: (old.stinger != new.stinger).then(|| api::StingerTransition {
            source: new.stinger.source.map(|id| id.get()),
            cut_ms: new.stinger.cut_ms,
            cut_found_ms: None,
        }),
    }
}

fn transition_kind(k: rbs_scene::TransitionKind) -> api::TransitionKind {
    match k {
        rbs_scene::TransitionKind::Cut => api::TransitionKind::Cut,
        rbs_scene::TransitionKind::Fade => api::TransitionKind::Fade,
        rbs_scene::TransitionKind::Swipe => api::TransitionKind::Swipe,
        rbs_scene::TransitionKind::Slide => api::TransitionKind::Slide,
        rbs_scene::TransitionKind::Stinger => api::TransitionKind::Stinger,
        _ => api::TransitionKind::Move,
    }
}

fn easing(e: rbs_scene::GlideEasing) -> api::GlideEasing {
    match e {
        rbs_scene::GlideEasing::Linear => api::GlideEasing::Linear,
        _ => api::GlideEasing::EaseInOut,
    }
}

fn swipe(s: rbs_scene::SwipeSettings) -> api::SwipeTransition {
    api::SwipeTransition {
        duration_ms: s.duration_ms,
        easing: easing(s.easing),
        direction: match s.direction {
            rbs_scene::SwipeDirection::Right => api::SwipeDirection::Right,
            rbs_scene::SwipeDirection::Up => api::SwipeDirection::Up,
            rbs_scene::SwipeDirection::Down => api::SwipeDirection::Down,
            _ => api::SwipeDirection::Left,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transition_patch_carries_only_the_parts_that_changed() {
        let old = rbs_scene::CollectionTransition::default();
        let mut new = old;
        new.fade.via = Some([0x1a, 0x2b, 0x3c]);
        let p = transition_patch(&old, &new);
        assert_eq!(p.fade.and_then(|f| f.via).as_deref(), Some("#1a2b3c"));
        assert!(p.kind.is_none() && p.direct_kind.is_none() && p.swipe.is_none());
        assert!(p.slide.is_none() && p.stinger.is_none());
        assert_eq!(
            transition_patch(&old, &old),
            rq::SetCollectionTransition::default()
        );
    }
}
