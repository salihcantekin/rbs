//! Looks: presets of an item's style and its source's filter chain (ADR-0075, spec
//! `scene-model.md`, "Looks"). A look is copied onto an item when it is applied; it is
//! not linked to it afterwards.
//!
//! This module holds the data and the built-in looks. Filter type ids and setting keys
//! are those of the filter catalogue (`rbs-filters`); this crate does not depend on it,
//! so `rbs-sources` and `rbs-app` test that every built-in look is accepted by it.

use crate::model::{Border, ItemStyle, SettingValue, Settings, Shape};

/// Type id of the colour correction filter.
const COLOR_CORRECTION: &str = "org.rbs.filter.color-correction";
/// Type id of the blur filter.
const BLUR: &str = "org.rbs.filter.blur";
/// Type id of the sharpen filter.
const SHARPEN: &str = "org.rbs.filter.sharpen";
/// Type id of the chroma key filter.
const CHROMA_KEY: &str = "org.rbs.filter.chroma-key";

/// How the Library groups looks.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookGroup {
    /// Colour grading.
    Color,
    /// Blur, sharpen, keying.
    Effect,
    /// The shape and border of the picture.
    Shape,
}

/// One filter a look puts on a source: a type and its settings.
#[derive(Debug, Clone, PartialEq)]
pub struct LookFilter {
    /// The filter type id.
    pub type_id: &'static str,
    /// Its settings; keys it leaves out keep the type's defaults.
    pub settings: Settings,
}

/// A preset of an item's style and its source's filters.
#[derive(Debug, Clone, PartialEq)]
pub struct Look {
    /// Stable id, `builtin:<slug>` for built-in looks.
    pub id: &'static str,
    /// Name for lists.
    pub name: &'static str,
    /// Its group.
    pub group: LookGroup,
    /// The style it sets on the item, if it sets one.
    pub style: Option<ItemStyle>,
    /// The filters it sets on the source: one of each type, replacing the source's
    /// filter of that type or added at the end of the chain.
    pub filters: Vec<LookFilter>,
}

fn numbers(pairs: &[(&str, f64)]) -> Settings {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), SettingValue::Float(*v)))
        .collect()
}

fn color(name: &'static str, id: &'static str, pairs: &[(&str, f64)]) -> Look {
    Look {
        id,
        name,
        group: LookGroup::Color,
        style: None,
        filters: vec![LookFilter {
            type_id: COLOR_CORRECTION,
            settings: numbers(pairs),
        }],
    }
}

fn effect(name: &'static str, id: &'static str, type_id: &'static str, s: Settings) -> Look {
    Look {
        id,
        name,
        group: LookGroup::Effect,
        style: None,
        filters: vec![LookFilter {
            type_id,
            settings: s,
        }],
    }
}

fn shape(name: &'static str, id: &'static str, style: ItemStyle) -> Look {
    Look {
        id,
        name,
        group: LookGroup::Shape,
        style: Some(style),
        filters: Vec::new(),
    }
}

/// The built-in looks, in the order the Library lists them. Lengths of shapes are in
/// the item's box units (canvas pixels for an item placed directly on the canvas).
pub fn built_in() -> Vec<Look> {
    let white = [255, 255, 255, 255];
    vec![
        color(
            "Warm",
            "builtin:warm",
            &[("hue", -8.0), ("saturation", 1.1), ("brightness", 0.03)],
        ),
        color(
            "Cool",
            "builtin:cool",
            &[("hue", 10.0), ("saturation", 1.05), ("brightness", -0.02)],
        ),
        color(
            "Vivid",
            "builtin:vivid",
            &[("saturation", 1.5), ("contrast", 0.15)],
        ),
        color(
            "Muted",
            "builtin:muted",
            &[("saturation", 0.6), ("contrast", -0.05)],
        ),
        color(
            "Black and white",
            "builtin:black-and-white",
            &[("saturation", 0.0), ("contrast", 0.1)],
        ),
        color(
            "Soft contrast",
            "builtin:soft-contrast",
            &[("contrast", 0.2), ("gamma", 1.1)],
        ),
        effect(
            "Soft blur",
            "builtin:soft-blur",
            BLUR,
            numbers(&[("radius", 6.0)]),
        ),
        effect("Sharpen", "builtin:sharpen", SHARPEN, numbers(&[])),
        effect(
            "Green screen",
            "builtin:green-screen",
            CHROMA_KEY,
            Settings::new(),
        ),
        shape("Circle", "builtin:circle", ItemStyle::new(Shape::Circle)),
        shape(
            "Circle with border",
            "builtin:circle-border",
            ItemStyle::new(Shape::Circle).with_border(Some(Border::new(8.0, white))),
        ),
        shape(
            "Rounded corners",
            "builtin:rounded",
            ItemStyle::new(Shape::Rectangle { radius: 32.0 }),
        ),
        shape(
            "Rounded with frame",
            "builtin:rounded-frame",
            ItemStyle::new(Shape::Rectangle { radius: 32.0 })
                .with_border(Some(Border::new(8.0, white))),
        ),
        shape("No shape", "builtin:plain", ItemStyle::default()),
    ]
}

/// The built-in look with this id.
pub fn find(id: &str) -> Option<Look> {
    built_in().into_iter().find(|l| l.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_built_in_and_styles_valid() {
        let looks = built_in();
        let mut ids: Vec<_> = looks.iter().map(|l| l.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), looks.len());
        for l in &looks {
            assert!(l.id.starts_with("builtin:"), "{}", l.id);
            assert!(
                l.style.is_some() || !l.filters.is_empty(),
                "{} does nothing",
                l.id
            );
            assert!(l.style.is_none_or(|s| s.is_valid()), "{}", l.id);
        }
        assert!(find("builtin:circle").is_some());
        assert!(find("nope").is_none());
    }
}
