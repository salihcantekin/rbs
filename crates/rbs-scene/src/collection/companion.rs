//! Atomic creation of a picture's companion sound (ADR-0186).

use std::collections::BTreeMap;

use rbs_graph::node::NodeTypeId;

use super::{Op, SceneCollection, SceneError, Undo, issue};
use crate::model::{
    COMPANION_OF_HINT, ItemContent, ItemId, Layout, SceneId, SceneItem, Settings, Source, SourceId,
};

impl SceneCollection {
    /// The unique scene placement of a picture for companion creation.
    ///
    /// This can be used as an early preflight; [`Self::add_companion`] checks again
    /// before committing. Multiple placements are ambiguous even across scenes.
    pub fn companion_picture_placement(
        &self,
        picture: SourceId,
    ) -> Result<(SceneId, ItemId), SceneError> {
        self.get_source(picture)?;
        let placements = self.placements_of(picture);
        match placements.as_slice() {
            [] => Err(SceneError::PictureNotPlaced(picture)),
            [placement] => Ok(*placement),
            _ => Err(SceneError::AmbiguousCompanion),
        }
    }

    /// Creates and places a picture's new companion as one atomic undoable edit.
    ///
    /// The companion is inserted immediately above the picture and inherits its group.
    /// `hints` may contain device information; the companion relation is set here.
    /// Failure leaves sources, placements and ID counters unchanged.
    pub fn add_companion(
        &mut self,
        picture: SourceId,
        name: &str,
        type_id: NodeTypeId,
        settings: Settings,
        mut hints: BTreeMap<String, String>,
        layout: Layout,
    ) -> Result<(SourceId, ItemId, Undo), SceneError> {
        let (scene, picture_item) = self.companion_picture_placement(picture)?;
        if self.sources().any(|s| s.companion_of() == Some(picture)) {
            return Err(SceneError::CompanionExists(picture));
        }
        let s = self.get_scene(scene)?;
        let index = s
            .items
            .iter()
            .position(|i| i.id == picture_item)
            .ok_or(SceneError::UnknownItem(picture_item))?;
        let group = s.items[index].group;
        let mut next = self.next.clone();
        let source_id = SourceId(issue(&mut next.source)?);
        let item_id = ItemId(issue(&mut next.item)?);
        hints.insert(COMPANION_OF_HINT.to_owned(), picture.get().to_string());
        let mut source = Source::new(source_id, name.trim(), type_id, settings);
        source.hints = hints;
        let mut item = SceneItem::new(item_id, ItemContent::Source(source_id), layout);
        item.group = group;
        let undo = self.apply_with(
            Op::InsertSource {
                source,
                placements: vec![(scene, index + 1, item)],
                levels: Vec::new(),
                sends: Vec::new(),
            },
            next,
        )?;
        Ok((source_id, item_id, undo))
    }
}
