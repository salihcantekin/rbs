//! The mapper registry (ADR-0131 decision 6): one mapper per OBS type id. A new RBS
//! type adds one mapper here and one fixture test; nothing else changes.

use std::collections::{BTreeMap, HashMap};

use rbs_graph::node::NodeTypeId;
use rbs_params::Field;
use rbs_scene::model::Settings;

use crate::env::{Catalogue, Lookup, LookupTarget};
use crate::obs::{ObsFilter, ObsSource};

pub mod filters;
pub mod sources;

/// What mappers may ask while they work.
pub struct MapContext<'a> {
    lookup: &'a dyn Lookup,
    catalogue: &'a dyn Catalogue,
    /// The OBS canvas size (`[Video] BaseCX`/`BaseCY`, or the fallback).
    pub canvas: (u32, u32),
    targets: HashMap<String, Vec<LookupTarget>>,
}

impl<'a> MapContext<'a> {
    /// A context for one import.
    pub fn new(lookup: &'a dyn Lookup, catalogue: &'a dyn Catalogue, canvas: (u32, u32)) -> Self {
        Self {
            lookup,
            catalogue,
            canvas,
            targets: HashMap::new(),
        }
    }

    /// The targets of `type_id`, asked from the lookup once per import.
    pub fn targets(&mut self, type_id: &str) -> &[LookupTarget] {
        let lookup = self.lookup;
        self.targets
            .entry(type_id.to_owned())
            .or_insert_with(|| lookup.targets(&NodeTypeId::new(type_id)))
    }

    /// The lookup.
    pub fn lookup(&self) -> &dyn Lookup {
        self.lookup
    }

    /// The field schema of an RBS type.
    pub fn fields(&self, type_id: &str) -> Option<&'static [Field]> {
        self.catalogue.fields(&NodeTypeId::new(type_id))
    }
}

/// A source as RBS will store it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct MappedSource {
    /// RBS source type.
    pub type_id: &'static str,
    /// Settings.
    pub settings: Settings,
    /// Hints for finding the device or window again (ADR-0077).
    pub hints: BTreeMap<String, String>,
    /// Picture size, when known; the canvas size is used otherwise.
    pub native: Option<(u32, u32)>,
    /// The type carries sound, so audio settings that RBS lacks are reported.
    pub has_audio: bool,
    /// Why the result is only approximate, one sentence each.
    pub notes: Vec<String>,
}

impl MappedSource {
    /// A source of `type_id` with no settings.
    pub fn new(type_id: &'static str) -> Self {
        Self {
            type_id,
            settings: Settings::new(),
            hints: BTreeMap::new(),
            native: None,
            has_audio: false,
            notes: Vec::new(),
        }
    }
}

/// The result of mapping one OBS source.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum SourceOutcome {
    /// Imported.
    Mapped(MappedSource),
    /// Not imported.
    Skipped {
        /// Why, one sentence.
        reason: String,
        /// What the user can do.
        hint: Option<String>,
    },
}

impl SourceOutcome {
    /// Skipped with no hint.
    pub fn skip(reason: impl Into<String>) -> Self {
        Self::Skipped {
            reason: reason.into(),
            hint: None,
        }
    }
}

/// Maps one OBS source type.
pub trait SourceMapper: Send + Sync {
    /// The OBS type id this mapper takes.
    fn obs_id(&self) -> &'static str;
    /// Every RBS type the mapper can produce; all must exist in the build.
    fn rbs_types(&self) -> &'static [&'static str];
    /// Map one source.
    fn map(&self, src: &ObsSource, ctx: &mut MapContext<'_>) -> SourceOutcome;
}

/// Whether a filter works on sound or on the picture, for the wizard's choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    /// An audio filter.
    Audio,
    /// A video filter.
    Video,
}

/// The result of mapping one OBS filter.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum FilterOutcome {
    /// An RBS filter.
    Filter {
        /// RBS filter type.
        type_id: &'static str,
        /// Settings.
        settings: Settings,
        /// Settings that name another source, by OBS name, resolved in pass two:
        /// `(settings key, OBS source name)`.
        source_refs: Vec<(String, String)>,
        /// Why the result is only approximate.
        notes: Vec<String>,
    },
    /// A crop filter: the source's region (ADR-0103), in source pixels.
    Region {
        /// Pixels cut from the left.
        left: u32,
        /// Pixels cut from the top.
        top: u32,
        /// Pixels cut from the right, or `None` when OBS gave a width instead.
        right: Option<u32>,
        /// Pixels cut from the bottom, or `None` when OBS gave a height instead.
        bottom: Option<u32>,
        /// The width and height OBS kept, when it stored a size rather than insets.
        size: Option<(u32, u32)>,
    },
    /// Not imported.
    Skipped {
        /// Why.
        reason: String,
    },
}

/// Maps one OBS filter type.
pub trait FilterMapper: Send + Sync {
    /// The OBS filter id this mapper takes.
    fn obs_id(&self) -> &'static str;
    /// Every RBS type the mapper can produce (empty for the source region).
    fn rbs_types(&self) -> &'static [&'static str];
    /// Audio or video.
    fn kind(&self) -> FilterKind;
    /// Map one filter.
    fn map(&self, filter: &ObsFilter, ctx: &mut MapContext<'_>) -> FilterOutcome;
}

/// The mappers an import uses.
pub struct Mappers {
    sources: BTreeMap<&'static str, Box<dyn SourceMapper>>,
    filters: BTreeMap<&'static str, Box<dyn FilterMapper>>,
}

impl Mappers {
    /// No mappers.
    pub fn empty() -> Self {
        Self {
            sources: BTreeMap::new(),
            filters: BTreeMap::new(),
        }
    }

    /// The mappers of ADR-0131 decision 7 and the ones added since.
    pub fn builtin() -> Self {
        let mut m = Self::empty();
        for s in sources::builtin() {
            m.register_source(s);
        }
        for f in filters::builtin() {
            m.register_filter(f);
        }
        m
    }

    /// Adds or replaces the mapper for its OBS id.
    pub fn register_source(&mut self, mapper: Box<dyn SourceMapper>) {
        self.sources.insert(mapper.obs_id(), mapper);
    }

    /// Adds or replaces the filter mapper for its OBS id.
    pub fn register_filter(&mut self, mapper: Box<dyn FilterMapper>) {
        self.filters.insert(mapper.obs_id(), mapper);
    }

    /// The mapper for an OBS source id.
    pub fn source(&self, obs_id: &str) -> Option<&dyn SourceMapper> {
        self.sources.get(obs_id).map(|b| &**b)
    }

    /// The mapper for an OBS filter id.
    pub fn filter(&self, obs_id: &str) -> Option<&dyn FilterMapper> {
        self.filters.get(obs_id).map(|b| &**b)
    }

    /// Every source mapper, by OBS id.
    pub fn sources(&self) -> impl Iterator<Item = &dyn SourceMapper> {
        self.sources.values().map(|b| &**b)
    }

    /// Every filter mapper, by OBS id.
    pub fn filters(&self) -> impl Iterator<Item = &dyn FilterMapper> {
        self.filters.values().map(|b| &**b)
    }
}

/// `true` when every RBS type of a mapper exists in the build.
pub(crate) fn available(types: &[&str], catalogue: &dyn Catalogue) -> bool {
    types.iter().all(|t| catalogue.has(&NodeTypeId::new(*t)))
}

/// OBS colour `0xAABBGGRR` as `#rrggbb`, or `#rrggbbaa` when not opaque.
pub fn obs_color(c: u64) -> String {
    let r = c & 0xFF;
    let g = (c >> 8) & 0xFF;
    let b = (c >> 16) & 0xFF;
    let a = (c >> 24) & 0xFF;
    if a == 0xFF {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn obs_colours_are_abgr() {
        assert_eq!(obs_color(0xFF00_00FF), "#ff0000");
        assert_eq!(obs_color(0xFF00_FF00), "#00ff00");
        assert_eq!(obs_color(0x80FF_0000), "#0000ff80");
    }
}
