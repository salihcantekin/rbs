//! The sources shown layers draw and the video filters turned on while their source
//! was on screen (`BindReport::turned_on_filters`, ADR-0199).

use std::collections::{BTreeMap, BTreeSet};

use rbs_graph::description::NodeId;

use super::{BindError, BoundSource, OutgoingScene, Role, role_cues};
use crate::collection::SceneCollection;
use crate::model::{CanvasId, FilterId, ItemId, SceneId, SourceId};

/// The sources a shown layer draws (flatten omits opacity 0); outgoing and stinger
/// sources keep their filters.
pub(super) fn drawn(
    collection: &SceneCollection,
    needed: &BTreeMap<(CanvasId, Role), SceneId>,
    cued_items: &BTreeSet<ItemId>,
    outgoing: &BTreeMap<CanvasId, &OutgoingScene>,
    running: &BTreeSet<SourceId>,
) -> Result<BTreeSet<SourceId>, BindError> {
    let mut drawn = BTreeSet::new();
    for (&(canvas, role), &scene) in needed {
        let cued_items = role_cues(canvas, role, cued_items, outgoing);
        let flat = collection
            .flatten_cued(canvas, scene, cued_items)
            .map_err(BindError::Scene)?;
        for layer in flat {
            if role == Role::Preview && !running.contains(&layer.source) {
                continue;
            }
            drawn.insert(layer.source);
        }
    }
    for o in outgoing.values() {
        drawn.extend(o.drawn.iter().copied());
        drawn.extend(o.stinger);
    }
    Ok(drawn)
}

/// Notes the video filters of `source` that join its chain in this sync.
pub(super) fn note_joined(
    joined: &mut Vec<(SourceId, FilterId, NodeId)>,
    is_audio: bool,
    source: SourceId,
    chain: &[FilterId],
    bound: &BoundSource,
) {
    if is_audio {
        return;
    }
    for f in chain.iter().filter(|f| !bound.chain.contains(f)) {
        if let Some(b) = bound.filters.get(f) {
            joined.push((source, *f, b.node));
        }
    }
}

/// The filters that joined a chain while their source was drawn before and after this
/// sync (`BindReport::turned_on_filters`); `before` becomes `now`.
pub(super) fn turned_on(
    joined: &[(SourceId, FilterId, NodeId)],
    before: &mut BTreeSet<SourceId>,
    now: &BTreeSet<SourceId>,
) -> Vec<(FilterId, NodeId)> {
    let on = joined
        .iter()
        .filter(|(s, ..)| before.contains(s) && now.contains(s))
        .map(|&(_, f, n)| (f, n))
        .collect();
    before.clone_from(now);
    on
}
