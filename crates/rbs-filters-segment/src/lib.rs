//! The background removal filter (ADR-0179, spec `person-mask-port.md` and
//! `video-filters.md`, "Background removal"): a video filter node that lends a person
//! mask session small pictures of its input and cuts the person out with the matte it
//! gets back. Never an adapter or a backend (the session comes from a factory), never a
//! full-resolution readback.

pub mod node;

pub use node::{BackgroundRemovalNode, RemovalStats, SessionFactory, descriptor};
use rbs_params::Field;

/// Type id of the filter.
pub const TYPE_ID: &str = "org.rbs.filter.background-removal";
/// English display name.
pub const NAME: &str = "Background removal";
/// Most fields the filter's schema has.
pub const MAX_FIELDS: usize = 8;

/// Matte value at the middle of the edge.
pub const THRESHOLD: usize = 0;
/// Width of the soft edge.
pub const FEATHER: usize = 1;
/// Moves the edge in (negative) or out (positive).
pub const EDGE_SHIFT: usize = 2;
/// How much of the matte before it each new matte keeps.
pub const SMOOTHING: usize = 3;

const FIELDS: [Field; 4] = [
    Field::number("threshold", "Threshold", 0.05, 0.95, 0.5, 0.01, "")
        .with_help("Matte value that counts as the edge of the person."),
    Field::number("feather", "Edge softness", 0.0, 0.5, 0.15, 0.01, "")
        .with_help("How soft the edge of the person is."),
    Field::number("edge_shift", "Edge shift", -1.0, 1.0, 0.0, 0.01, "")
        .with_help("Moves the edge into the person (negative) or out into the room (positive)."),
    Field::number("smoothing", "Smoothing", 0.0, 0.95, 0.5, 0.01, "")
        .with_help("Calms flicker between pictures; a higher value reacts more slowly."),
];

/// The settings schema, in display order.
pub fn fields() -> &'static [Field] {
    &FIELDS
}

/// The uniform values of the composite pass for validated settings: the matte values
/// where the picture starts to show and where it is whole.
pub fn edges(threshold: f32, feather: f32, shift: f32) -> [f32; 2] {
    let centre = threshold - 0.3 * shift;
    [
        (centre - feather).max(0.0),
        (centre + feather.max(0.002)).min(1.0),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_is_valid() {
        assert!(rbs_params::check_fields(fields(), MAX_FIELDS).is_ok());
        assert!(rbs_graph::node::NodeTypeId::new_static(TYPE_ID).is_well_formed());
    }

    #[test]
    fn the_edge_moves_with_the_shift() {
        let [lo, hi] = edges(0.5, 0.1, 0.0);
        assert!((lo - 0.4).abs() < 1e-6 && (hi - 0.6).abs() < 1e-6);
        let [lo2, _] = edges(0.5, 0.1, 1.0);
        assert!(lo2 < lo, "a positive shift lets more of the matte through");
        let [a, b] = edges(0.5, 0.0, 0.0);
        assert!(b > a, "a hard edge still has a width");
    }
}
