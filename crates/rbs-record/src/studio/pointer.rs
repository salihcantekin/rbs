//! The studio's part of the pointer host service (ADR-0165 decisions 3 and 5; spec
//! `video-filters.md`, "Binding and the studio"): a pointer filter's instance is lent
//! the process's input monitor and its source entry's mapping cell, and each source
//! entry's mapping switch is on exactly while a scheduled filter of that source needs
//! the pointer. Control thread only.

use std::sync::Arc;

use rbs_filters::{FilterNode, FilterStats, FilterType, Needs, PointerFeed, Values};
use rbs_graph::runtime::param::ParamSender;
use rbs_scene::model::SourceId;
use rbs_sources::SourceTypes;

use super::{Entry, Studio, Watch};

/// A video filter's node for a filter of `source`, warmed for the source's picture
/// size when it is known, and lent the pointer when its type needs it and `sources`
/// has an input monitor; `true` when it was lent it.
pub(super) fn video_filter(
    entries: &[Entry],
    source: SourceId,
    ty: &'static dyn FilterType,
    values: Values,
    sources: &SourceTypes,
) -> (FilterNode, ParamSender<Values>, Arc<FilterStats>, bool) {
    let entry = entries.iter().find(|e| e.source == source);
    let size = entry
        .and_then(|e| match &e.instance.as_ref()?.watch {
            Watch::Video(stats) => stats.picture_size(),
            Watch::Text(stats) => stats.picture_size(),
            Watch::Browser { size } => Some(*size),
            Watch::Media(_) | Watch::Audio(_, _) => None,
        })
        .map(|(w, h)| [w, h]);
    let shared: Arc<dyn FilterType> = Arc::new(ty);
    let (node, sender, stats) = FilterNode::new(shared, values, size);
    let feed = ty
        .needs()
        .contains(Needs::POINTER)
        .then(|| {
            Some(PointerFeed::new(
                sources.input()?.clone(),
                entry?.mapping.cell.clone(),
            ))
        })
        .flatten();
    match feed {
        Some(feed) => (node.with_pointer(feed), sender, stats, true),
        None => (node, sender, stats, false),
    }
}

impl Studio {
    /// Sets every source entry's mapping switch to "a scheduled filter of this source
    /// was lent the pointer" (ADR-0165 decision 5), from the filters' `scheduled`
    /// flags. Without an input monitor no filter is lent it, so the adapter does no
    /// mapping work (P13). The switch does nothing when its state is unchanged.
    pub(super) fn set_mapping_switches(&self) {
        for e in &self.entries {
            let on = self
                .filters
                .iter()
                .any(|f| f.source == e.source && f.scheduled && f.fed && f.instance.is_some());
            e.mapping.switch.set(on);
        }
    }

    /// Whether the mapping switch of `source` is on: a scheduled filter of it was lent
    /// the pointer. `false` for a source that is not bound.
    pub fn pointer_mapping_on(&self, source: SourceId) -> bool {
        self.entries
            .iter()
            .any(|e| e.source == source && e.mapping.switch.is_on())
    }
}
