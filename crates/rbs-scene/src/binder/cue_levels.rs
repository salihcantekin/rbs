use super::*;

pub(super) fn program_sources(
    collection: &SceneCollection,
    canvas: CanvasId,
    scene: SceneId,
    live: &BTreeSet<ItemId>,
    outgoing: &BTreeMap<CanvasId, &OutgoingScene>,
) -> Result<BTreeSet<SourceId>, BindError> {
    let cues = role_cues(canvas, Role::Program, live, outgoing);
    Ok(collection
        .visible_sources_cued(canvas, scene, cues)
        .map_err(BindError::Scene)?
        .into_iter()
        .collect())
}

pub(super) fn source_levels(
    collection: &SceneCollection,
    cued_items: &BTreeSet<ItemId>,
    outgoing: &BTreeMap<CanvasId, &OutgoingScene>,
) -> Result<BTreeMap<SourceId, Level>, BindError> {
    if !outgoing.values().any(|o| o.program_cues.is_some()) {
        return Ok(collection.source_levels(cued_items));
    }
    let mut levels: BTreeMap<_, _> = collection
        .wanted_sources()
        .into_iter()
        .map(|id| (id, Level::Placed))
        .collect();
    for canvas in collection.canvases() {
        for (scene, role, level) in [
            (canvas.preview, Role::Preview, Level::Shown),
            (canvas.program, Role::Program, Level::OnAir),
        ] {
            if let Some(scene) = scene {
                let cues = role_cues(canvas.id, role, cued_items, outgoing);
                for id in collection
                    .visible_sources_cued(canvas.id, scene, cues)
                    .map_err(BindError::Scene)?
                {
                    let slot = levels.entry(id).or_insert(level);
                    *slot = (*slot).max(level);
                }
            }
        }
        if let Some(scene) = outgoing.get(&canvas.id).and_then(|o| o.incoming_scene) {
            for id in collection
                .visible_sources_cued(canvas.id, scene, cued_items)
                .map_err(BindError::Scene)?
            {
                let slot = levels.entry(id).or_insert(Level::Shown);
                *slot = (*slot).max(Level::Shown);
            }
        }
    }
    Ok(levels)
}
