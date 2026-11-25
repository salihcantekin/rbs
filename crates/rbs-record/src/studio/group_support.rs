//! Prospective follower resolution before committing a Studio collection update.

use rbs_scene::{
    SceneCollection,
    model::{CanvasId, SceneId},
};
use std::borrow::Cow;
use std::collections::BTreeMap;

pub(super) fn program_scene(collection: &SceneCollection, canvas: CanvasId) -> Option<SceneId> {
    collection.canvas(canvas).and_then(|canvas| canvas.program)
}

/// `collection` with every following canvas's program scene set to what it shows now
/// (ADR-0093 decision 3); `last` remembers it for the `KeepLast` fallback. The binder
/// sees only program scenes, so it needs to know nothing of following.
pub(super) fn with_followers<'a>(
    collection: &'a SceneCollection,
    last: &mut BTreeMap<CanvasId, SceneId>,
) -> Cow<'a, SceneCollection> {
    let shown = collection.followed_programs(last);
    if shown.is_empty() {
        return Cow::Borrowed(collection);
    }
    let mut resolved = collection.clone();
    for (canvas, scene) in shown {
        // An override the collection refuses leaves the canvas on its own program.
        if resolved.set_program(canvas, Some(scene)).is_ok() {
            last.insert(canvas, scene);
        }
    }
    Cow::Owned(resolved)
}
