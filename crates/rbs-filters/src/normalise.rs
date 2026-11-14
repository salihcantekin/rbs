//! The normalisation pass of a separate video file (ADR-0086 decision 9): a filter with
//! no settings that draws its input into the canvas format, linear light and
//! premultiplied, with the decode the compositor uses for the canvas. An ISO track of a
//! source with no filters reads the source node's own picture, which may be 8-bit RGB;
//! this pass gives the export the only input it takes. Not in the catalogue: users never
//! add it.

use rbs_graph::node::NodeTypeId;

use crate::types::{Field, FilterType, Pass, PassSource, Scale, Uniforms, Values};

/// The type id of [`Normalise`]; not a catalogue entry.
pub const NORMALISE: &str = "org.rbs.filter.normalise";

/// Draws its input into the canvas format, unchanged.
#[derive(Debug, Clone, Copy, Default)]
pub struct Normalise;

const PASSES: [Pass; 1] = [Pass {
    shader: include_str!("shaders/normalise.wgsl"),
    target: Scale::Full,
    source: PassSource::Input,
}];

impl FilterType for Normalise {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new_static(NORMALISE)
    }
    fn name(&self) -> &'static str {
        "Normalise"
    }
    fn fields(&self) -> &'static [Field] {
        &[]
    }
    fn passes(&self) -> &'static [Pass] {
        &PASSES
    }
    fn uniforms(&self, _values: &Values, _input: [u32; 2], _out: &mut [Uniforms]) {}
}
