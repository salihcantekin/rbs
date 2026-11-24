//! The undo and redo step shown to the window.

use rbs_app::edit_history::{EditKind, StepInfo};

/// The Studio global's `undo-kind` / `redo-kind` code and the source's name for a
/// step: 0 nothing, 1 add, 2 delete, 3 layer order, 4 move or resize, 5 shape or
/// border, 6 crop, 7 shown or hidden, 8 lock, 9 source rename, 10 scene added, 11
/// scene removed, 12 scene renamed (the name is the scene's, or the old name), 13
/// filters changed, 14 removed from one scene, 15 made global or moved back into a
/// scene, 16 look applied, 17 source pointed at another target, 18 source settings
/// changed, 19 flipped, turned, fitted or centred, 20 glide duration or easing,
/// 21 how long Show draws a hidden item (the cue itself is not an undo step),
/// 22 the source's capture region, 23 the source's Record separately mark, 24 a
/// scene's levels (ADR-0091; the name is the scene's), 25 a source duplicated, 26 a
/// source reset, 27 attributes pasted (ADR-0133; the copied source's name), 28 the
/// collection's transition, 29 Preview edits dropped when studio mode ended (ADR-0151;
/// the scene's name), 30 the scroll (ADR-0150), 31 grouped, 32 ungrouped, 33 group
/// renamed (ADR-0182; the group's name).
pub(super) fn step_view(step: Option<&StepInfo>) -> (i32, &str) {
    match step {
        None => (0, ""),
        Some(s) => (
            match s.kind {
                EditKind::Add => 1,
                EditKind::Delete => 2,
                EditKind::Order => 3,
                EditKind::Place => 4,
                EditKind::Style => 5,
                EditKind::Crop => 6,
                EditKind::Visibility => 7,
                EditKind::Lock => 8,
                EditKind::RenameSource => 9,
                EditKind::AddScene => 10,
                EditKind::RemoveScene => 11,
                EditKind::RenameScene => 12,
                EditKind::Filter => 13,
                EditKind::Remove => 14,
                EditKind::Global => 15,
                EditKind::Look => 16,
                EditKind::Retarget => 17,
                EditKind::SourceSettings => 18,
                EditKind::Transform => 19,
                EditKind::Glide => 20,
                EditKind::ShowFor => 21,
                EditKind::Region => 22,
                EditKind::RecordSeparately => 23,
                EditKind::SceneLevels => 24,
                EditKind::Duplicate => 25,
                EditKind::Reset => 26,
                EditKind::PasteAttributes => 27,
                EditKind::Transition => 28,
                EditKind::DiscardPreview => 29,
                EditKind::Scroll => 30,
                EditKind::Group => 31,
                EditKind::Ungroup => 32,
                EditKind::RenameGroup => 33,
            },
            s.name.as_str(),
        ),
    }
}
