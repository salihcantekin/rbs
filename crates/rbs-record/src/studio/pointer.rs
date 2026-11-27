//! The studio's part of the pointer host service (ADR-0165 decisions 3 and 5; spec
//! `video-filters.md`, "Binding and the studio"): a pointer filter's instance is lent
//! the process's input monitor and its source entry's mapping cell, and each source
//! entry's mapping switch is on exactly while a scheduled filter of that source needs
//! the pointer. Control thread only.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rbs_audio_filters::{AudioFilterStats, FilterMeter};
use rbs_filters::{FilterNode, FilterStats, FilterType, Needs, PointerFeed, Values};
use rbs_gpu::node::GpuRuntime;
use rbs_graph::runtime::Node;
use rbs_graph::runtime::param::ParamSender;
use rbs_scene::binder::BindReport;
use rbs_scene::model::SourceId;
use rbs_sources::SourceTypes;

use super::{Entry, FilterEntry, Studio, Watch};

/// A running filter: the value channel the studio keeps and what the node shares
/// (ADR-0062 decision 8; ADR-0082 decision 8 for audio filters).
pub(super) enum FilterInstance {
    /// A video filter on the render partition.
    /// The last field is the node's turned-on flag, when its type has one (ADR-0199).
    Video(
        ParamSender<Values>,
        Arc<FilterStats>,
        Option<Arc<AtomicBool>>,
    ),
    /// An audio filter on the audio partition, with its meter if it has one.
    Audio {
        sender: ParamSender<Values>,
        meter: Option<Arc<FilterMeter>>,
        stats: Arc<AudioFilterStats>,
    },
}

impl FilterInstance {
    pub(super) fn sender(&mut self) -> &mut ParamSender<Values> {
        match self {
            FilterInstance::Video(sender, ..) | FilterInstance::Audio { sender, .. } => sender,
        }
    }
}

/// A video filter's node for a filter of `source`, warmed for the source's picture
/// size when it is known, and lent the pointer when its type needs it and `sources`
/// has an input monitor; `filter.fed` says whether it was lent it.
pub(super) fn video_filter(
    entries: &[Entry],
    filter: &mut FilterEntry,
    ty: &'static dyn FilterType,
    values: Values,
    sources: &SourceTypes,
) -> (Box<dyn Node<GpuRuntime>>, FilterInstance) {
    let source = filter.source;
    let entry = entries.iter().find(|e| e.source == source);
    let size = entry
        .and_then(|e| match &e.instance.as_ref()?.watch {
            Watch::Video(stats) => stats.picture_size(),
            Watch::Text(stats) => stats.picture_size(),
            Watch::Sized { size } => Some(*size),
            Watch::Media(_) | Watch::Audio(_, _) => None,
        })
        .map(|(w, h)| [w, h]);
    // A type with its own node (background removal, ADR-0179) builds it; the generic
    // pass node runs every other type.
    if let Some(own) = ty.custom_node(values, size) {
        let instance = FilterInstance::Video(own.values, own.stats, own.turned_on);
        filter.fed = false;
        return (own.node, instance);
    }
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
    let (node, fed): (Box<dyn Node<GpuRuntime>>, bool) = match feed {
        Some(feed) => (Box::new(node.with_pointer(feed)), true),
        None => (Box::new(node), false),
    };
    filter.fed = fed;
    (node, FilterInstance::Video(sender, stats, None))
}

impl Studio {
    /// Sets the turned-on flag of each video filter the bind reported turned on while
    /// its source was on screen, before the apply that schedules it (ADR-0199).
    pub(super) fn mark_turned_on(&self, report: Option<&BindReport>) {
        let Some(report) = report else { return };
        for entry in &self.filters {
            if let Some(FilterInstance::Video(_, _, Some(flag))) = &entry.instance
                && report
                    .turned_on_filters
                    .iter()
                    .any(|&(_, n)| n == entry.node)
            {
                flag.store(true, Ordering::Release);
            }
        }
    }

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
