//! Attribute groups (ADR-0133 decision 1): the one vocabulary for Reset source and
//! Paste attributes, in the engine, the control API and the UI.
//!
//! This module holds only the names and the classification. What a group's default is
//! and whether it applies to a source depend on node descriptors, source classes and the
//! filter catalogue, which the host knows; reset and paste are host compositions.

/// A named part of a source and its item that can be reset or pasted on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AttributeGroup {
    /// The item's position, size and rotation.
    Transform,
    /// The item's crop.
    Crop,
    /// The item's opacity.
    Opacity,
    /// The item's shape, border and shadow.
    Style,
    /// The item's scroll (ADR-0150).
    Scroll,
    /// The source's picture filters, in order.
    VideoFilters,
    /// The source's audio filters, in order.
    AudioFilters,
    /// The source type's option keys (not its identity keys).
    Options,
    /// The capture region, and Display chrome for a display source.
    Region,
    /// The source's glide override.
    Glide,
    /// The Record separately feeds the target has.
    RecordSeparately,
    /// Volume, mute and monitor: live, never part of an undo step.
    Sound,
    /// Sync offset: live, never part of an undo step.
    SyncOffset,
}

impl AttributeGroup {
    /// Every group, in the order the dialogs list them.
    pub const ALL: [Self; 13] = [
        Self::Transform,
        Self::Crop,
        Self::Opacity,
        Self::Style,
        Self::Scroll,
        Self::VideoFilters,
        Self::AudioFilters,
        Self::Options,
        Self::Region,
        Self::Glide,
        Self::RecordSeparately,
        Self::Sound,
        Self::SyncOffset,
    ];

    /// The id used in the control API.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Transform => "transform",
            Self::Crop => "crop",
            Self::Opacity => "opacity",
            Self::Style => "style",
            Self::Scroll => "scroll",
            Self::VideoFilters => "video_filters",
            Self::AudioFilters => "audio_filters",
            Self::Options => "options",
            Self::Region => "region",
            Self::Glide => "glide",
            Self::RecordSeparately => "record_separately",
            Self::Sound => "sound",
            Self::SyncOffset => "sync_offset",
        }
    }

    /// The group named `id`.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|g| g.id() == id)
    }

    /// The UI name (glossary terms).
    pub const fn title(self) -> &'static str {
        match self {
            Self::Transform => "Position and size",
            Self::Crop => "Crop",
            Self::Opacity => "Opacity",
            Self::Style => "Shape and border",
            Self::Scroll => "Scroll",
            Self::VideoFilters => "Picture filters",
            Self::AudioFilters => "Sound filters",
            Self::Options => "Source options",
            Self::Region => "Capture region",
            Self::Glide => "Glide",
            Self::RecordSeparately => "Record separately",
            Self::Sound => "Volume and monitoring",
            Self::SyncOffset => "Sync offset",
        }
    }

    /// Written as a live control outside the undo step, and never chosen by default.
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Sound | Self::SyncOffset)
    }

    /// Lives on the item's layout rather than on the source.
    pub const fn on_item(self) -> bool {
        matches!(
            self,
            Self::Transform | Self::Crop | Self::Opacity | Self::Style | Self::Scroll
        )
    }

    /// The groups chosen when a caller names none: every non-live group.
    pub fn default_choice() -> Vec<Self> {
        Self::ALL.into_iter().filter(|g| !g.is_live()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::AttributeGroup;
    use crate::model::{
        Crop, ItemStyle, Layout, RecordSeparately, SceneItem, Scroll, Source, SourceAudio,
    };

    #[test]
    fn ids_round_trip_and_are_unique() {
        for g in AttributeGroup::ALL {
            assert_eq!(AttributeGroup::from_id(g.id()), Some(g));
        }
        let mut ids: Vec<_> = AttributeGroup::ALL.iter().map(|g| g.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), AttributeGroup::ALL.len());
        assert!(
            AttributeGroup::default_choice()
                .iter()
                .all(|g| !g.is_live())
        );
    }

    /// Every user-set field is in a group or on the "never" list (ADR-0133 decision 1).
    /// A new field breaks this exhaustive destructuring: classify it in the ADR's table,
    /// the spec and here.
    #[test]
    fn every_field_is_classified() {
        #[allow(unused_variables, dead_code)]
        fn source(s: Source) {
            let Source {
                id,                // never
                name,              // never
                type_id,           // never
                settings,          // Options (option keys) / never (identity keys)
                filters,           // VideoFilters, AudioFilters
                audio,             // see SourceAudio
                glide,             // Glide
                hints,             // never
                region,            // Region
                chrome,            // Region
                record_separately, // RecordSeparately
                activity,          // never (ADR-0152; Duplicate copies it)
            } = s;
        }
        #[allow(unused_variables, dead_code)]
        fn audio(a: SourceAudio) {
            let SourceAudio {
                volume,         // Sound
                muted,          // Sound
                sync_offset_ms, // SyncOffset
                mixer_hidden,   // never (a view choice)
                monitor,        // Sound
            } = a;
        }
        #[allow(unused_variables, dead_code)]
        fn item(i: SceneItem) {
            let SceneItem {
                id,          // never
                content,     // never
                visible,     // never
                show_for_ms, // never
                locked,      // never
                layout,      // see Layout
                overrides,   // the canvas rule
                group,       // never (a group is not copied or reset, ADR-0182)
            } = i;
        }
        #[allow(unused_variables, dead_code)]
        fn layout(l: Layout) {
            let Layout {
                position, // Transform
                size,     // Transform
                rotation, // Transform
                crop,     // Crop
                opacity,  // Opacity
                style,    // Style
                scroll,   // Scroll
            } = l;
        }
        #[allow(unused_variables, dead_code)]
        fn record(r: RecordSeparately) {
            let RecordSeparately {
                video,    // RecordSeparately
                audio,    // RecordSeparately
                video_as, // RecordSeparately
            } = r;
        }
        #[allow(unused_variables, dead_code)]
        fn style(s: ItemStyle) {
            let ItemStyle {
                shape,  // Style
                border, // Style
                shadow, // Style
            } = s;
        }
        #[allow(unused_variables, dead_code)]
        fn scroll(s: Scroll) {
            let Scroll {
                direction, // Scroll
                speed,     // Scroll
                gap,       // Scroll
                repeat,    // Scroll
                start,     // Scroll
                fade,      // Scroll
                ramp_ms,   // Scroll
                on_return, // Scroll
            } = s;
        }
        #[allow(unused_variables, dead_code)]
        fn crop(c: Crop) {
            let Crop {
                left,   // Crop
                top,    // Crop
                right,  // Crop
                bottom, // Crop
            } = c;
        }
    }
}
