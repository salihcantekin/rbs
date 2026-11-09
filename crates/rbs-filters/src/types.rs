//! The filter type contract: schema, values, passes and uniforms (spec
//! `video-filters.md`, "Filter types" and "Values").

use rbs_graph::node::NodeTypeId;
pub use rbs_params::{
    Field, FieldKind, ParamError, Schema, Value, Values, color_text, parse_color,
};

/// Most fields a video filter type may have (the shared schema allows
/// [`rbs_params::MAX_FIELDS`]).
pub const MAX_FIELDS: usize = 8;
/// Most passes a filter type may have.
pub const MAX_PASSES: usize = 2;
/// `vec4<f32>` entries in one pass's uniform block; the last one is the node's.
pub const UNIFORM_VECS: usize = 16;

/// A filter: its schema, its shaders and how its values become uniforms.
///
/// Implementations are pure descriptions; [`crate::FilterNode`] runs any of them. A
/// plugin filter or a test double implements this trait and the node, the binder and
/// the application do not change (ADR-0062 decision 1).
pub trait FilterType: Send + Sync + 'static {
    /// `org.rbs.filter.<name>` for built-in types; well-formed.
    fn type_id(&self) -> NodeTypeId;
    /// English display name, for example "Colour correction".
    fn name(&self) -> &'static str;
    /// The settings schema, in display order; at most [`MAX_FIELDS`].
    fn fields(&self) -> &'static [Field];
    /// The passes, in order: 1 to [`MAX_PASSES`]. The first samples
    /// [`PassSource::Input`]; the last targets [`Scale::Full`].
    fn passes(&self) -> &'static [Pass];
    /// Fills one uniform block per pass (`out.len() == passes().len()`) from validated
    /// `values` and the input's size in pixels. Entries `0..15` are the type's; the
    /// node writes entry 15. Pure: no I/O, no allocation; called on the render thread
    /// when the values or the size change.
    fn uniforms(&self, values: &Values, input: [u32; 2], out: &mut [Uniforms]);
}

/// A catalogue entry used where an owned type is wanted: `Arc::new(entry)`.
impl FilterType for &'static dyn FilterType {
    fn type_id(&self) -> NodeTypeId {
        (**self).type_id()
    }
    fn name(&self) -> &'static str {
        (**self).name()
    }
    fn fields(&self) -> &'static [Field] {
        (**self).fields()
    }
    fn passes(&self) -> &'static [Pass] {
        (**self).passes()
    }
    fn uniforms(&self, values: &Values, input: [u32; 2], out: &mut [Uniforms]) {
        (**self).uniforms(values, input, out);
    }
}

/// Size of a pass's target relative to the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    /// The input's size.
    Full,
    /// Half the input's size, each side rounded up.
    Half,
}

impl Scale {
    /// The target size for an input of `input` pixels.
    pub fn of(self, input: [u32; 2]) -> [u32; 2] {
        match self {
            Scale::Full => input,
            Scale::Half => [input[0].div_ceil(2).max(1), input[1].div_ceil(2).max(1)],
        }
    }
}

/// What a pass samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassSource {
    /// The node's input frame.
    Input,
    /// The previous pass's target.
    Previous,
}

/// One full-target draw of a filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pass {
    /// WGSL defining `@fragment fn fs_main(in: FilterIn) -> @location(0) vec4<f32>`
    /// with the prelude's bindings ([`crate::PRELUDE`]).
    pub shader: &'static str,
    /// Size of the target.
    pub target: Scale,
    /// What it samples.
    pub source: PassSource,
}

/// One pass's uniform block: 16 `vec4<f32>` (256 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Uniforms(pub [[f32; 4]; UNIFORM_VECS]);

impl Default for Uniforms {
    fn default() -> Self {
        Self([[0.0; 4]; UNIFORM_VECS])
    }
}

/// A filter type's schema is its field list, so [`Values`] works on it directly.
impl Schema for dyn FilterType {
    fn fields(&self) -> &'static [Field] {
        FilterType::fields(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_scale_rounds_up() {
        assert_eq!(Scale::Half.of([1921, 1081]), [961, 541]);
        assert_eq!(Scale::Half.of([1, 1]), [1, 1]);
        assert_eq!(Scale::Full.of([7, 3]), [7, 3]);
    }
}
