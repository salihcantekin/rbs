//! Pointer filters in a source's chain (ADR-0165 decision 3; spec `video-filters.md`,
//! "Pointer types"): every filter type with `Needs::POINTER` stays before every other
//! filter, so effects are drawn in source pixels before anything moves them; and such a
//! filter is added only to a source that publishes a picture mapping. Both come from
//! declared properties (`needs()`, `capture.mapping`), never from a type id (rule 19).

use rbs_filters::Needs;
use rbs_graph::node::NodeTypeId;
use rbs_scene::Undo;
use rbs_scene::model::{FilterId, SourceId};

use super::{Host, Refusal};

/// Whether filters of `type_id` need the pointer.
pub(super) fn is_pointer(type_id: &NodeTypeId) -> bool {
    is_pointer_str(type_id.as_str())
}

/// [`is_pointer`] for a type id as text.
pub(super) fn is_pointer_str(type_id: &str) -> bool {
    rbs_filters::find(type_id).is_some_and(|t| t.needs().contains(Needs::POINTER))
}

impl Host {
    /// Whether the window's `call` adds a pointer filter while Input monitoring was
    /// not asked yet in this run and can show the system's prompt.
    pub(super) fn pointer_prompt_first(&self, call: &rbs_api::Command) -> bool {
        use rbs_sources::permissions::{PermissionKind, PermissionStatus};
        let rbs_api::Command::FilterAdd(add) = call else {
            return false;
        };
        let kind = PermissionKind::InputMonitoring;
        if !is_pointer_str(&add.type_id) || self.pickers.permission_asked.contains(&kind) {
            return false;
        }
        let info = self.adapters.sources.permission(kind);
        info.status == PermissionStatus::NotAsked && info.can_prompt
    }

    /// Refuses a pointer filter on a source that publishes no picture mapping: it
    /// could never draw there.
    pub(super) fn check_pointer_source(
        &self,
        source: SourceId,
        type_id: &NodeTypeId,
        name: &str,
    ) -> Result<(), Refusal> {
        if !is_pointer(type_id) {
            return Ok(());
        }
        let mapped = self
            .collection
            .source(source)
            .is_some_and(|s| self.adapters.sources.publishes_mapping(&s.type_id));
        if mapped {
            Ok(())
        } else {
            Err(Refusal::Invalid(format!(
                "{name} needs a Screen capture or Window capture that can follow the pointer."
            )))
        }
    }

    /// Moves `source`'s pointer filters to the front of its chain, in their order;
    /// one undo record per move. Nothing when they are first already.
    pub(super) fn pointer_filters_first(
        &mut self,
        source: SourceId,
        records: &mut Vec<Undo>,
    ) -> Result<(), Refusal> {
        let pointers: Vec<FilterId> = self
            .collection
            .source(source)
            .map(|s| {
                s.filters
                    .iter()
                    .filter(|f| is_pointer(&f.type_id))
                    .map(|f| f.id)
                    .collect()
            })
            .unwrap_or_default();
        for (target, id) in pointers.into_iter().enumerate() {
            let at = self
                .collection
                .source(source)
                .and_then(|s| s.filters.iter().position(|f| f.id == id));
            if at != Some(target) {
                let undo = self
                    .collection
                    .move_filter(source, id, target)
                    .map_err(|e| Refusal::of(&e))?;
                records.push(undo);
            }
        }
        Ok(())
    }
}

/// The position `index` asked for a filter, kept inside its part of the chain:
/// pointer filters before the others. `pointers` counts the chain's pointer filters
/// other than the one moved, `len` the chain's length.
pub(super) fn clamp_move(pointer: bool, index: usize, pointers: usize, len: usize) -> usize {
    if pointer {
        index.min(pointers)
    } else {
        index.max(pointers).min(len.saturating_sub(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_stays_in_its_part_of_the_chain() {
        // Chain of 4, one other pointer filter at 0.
        assert_eq!(
            clamp_move(true, 3, 1, 4),
            1,
            "a pointer filter stays up front"
        );
        assert_eq!(clamp_move(true, 0, 1, 4), 0);
        assert_eq!(clamp_move(false, 0, 1, 4), 1, "another filter stays behind");
        assert_eq!(clamp_move(false, 3, 1, 4), 3);
        // A chain of one, all pointer filters, none.
        assert_eq!(clamp_move(true, 0, 0, 1), 0);
        assert_eq!(clamp_move(false, 0, 0, 1), 0);
        assert_eq!(clamp_move(true, 2, 2, 3), 2, "all pointer: free");
        assert_eq!(clamp_move(false, 1, 0, 3), 1, "none: unchanged");
        assert!(is_pointer(&NodeTypeId::new_static(
            rbs_filters::CLICK_RINGS
        )));
        assert!(!is_pointer(&NodeTypeId::new_static(
            rbs_filters::catalogue::BLUR
        )));
    }
}
