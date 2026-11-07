//! The filter type contract: schema, values, passes and uniforms (spec
//! `video-filters.md`, "Filter types" and "Values").

use std::fmt;

use rbs_graph::node::NodeTypeId;

/// Most fields a filter type may have.
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

/// One setting of a filter type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Field {
    /// Settings key: lowercase ASCII letters, digits and `_`.
    pub key: &'static str,
    /// English label for the UI.
    pub label: &'static str,
    /// What it holds.
    pub kind: FieldKind,
}

/// The kind of a field and its range or default.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldKind {
    /// A number in `min..=max`.
    Number {
        /// Smallest value.
        min: f32,
        /// Largest value.
        max: f32,
        /// Value when not set.
        default: f32,
        /// The UI's increment.
        step: f32,
        /// Shown after the value ("px", "°"); may be empty.
        unit: &'static str,
    },
    /// An sRGB colour with straight alpha.
    Color {
        /// Value when not set.
        default: [u8; 4],
    },
}

impl FieldKind {
    /// The default value.
    pub fn default_value(&self) -> Value {
        match *self {
            FieldKind::Number { default, .. } => Value::Number(default),
            FieldKind::Color { default } => Value::Color(default),
        }
    }
}

/// One setting's value.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    /// A number.
    Number(f32),
    /// An sRGB colour with straight alpha.
    Color([u8; 4]),
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

/// Why values were refused.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterError {
    /// The type has no field with this key.
    UnknownKey(String),
    /// A colour for a number, a number for a colour, or a value of another kind.
    WrongKind(&'static str),
    /// A number outside its field's range, or not finite.
    OutOfRange(&'static str),
    /// No filter type with this id.
    UnknownType(String),
}

impl fmt::Display for FilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilterError::UnknownKey(k) => write!(f, "no setting named {k:?}"),
            FilterError::WrongKind(k) => write!(f, "setting {k:?} has the wrong kind of value"),
            FilterError::OutOfRange(k) => write!(f, "setting {k:?} is out of range"),
            FilterError::UnknownType(t) => write!(f, "no filter type {t:?}"),
        }
    }
}

impl std::error::Error for FilterError {}

/// One valid value per field of a filter type, in field order. Fixed size, so it moves
/// through a parameter channel without allocating (ADR-0023).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Values {
    len: usize,
    items: [Value; MAX_FIELDS],
}

impl Values {
    /// Every field's default.
    pub fn defaults(ty: &dyn FilterType) -> Self {
        let mut items = [Value::Number(0.0); MAX_FIELDS];
        let fields = ty.fields();
        for (slot, field) in items.iter_mut().zip(fields) {
            *slot = field.kind.default_value();
        }
        Self {
            len: fields.len().min(MAX_FIELDS),
            items,
        }
    }

    /// The defaults with the given keys replaced; refused as a whole if any pair is.
    pub fn from_pairs<'k>(
        ty: &dyn FilterType,
        pairs: impl IntoIterator<Item = (&'k str, Value)>,
    ) -> Result<Self, FilterError> {
        Self::defaults(ty).with(ty, pairs)
    }

    /// These values with the given keys replaced (a patch); refused as a whole if any
    /// pair is.
    pub fn with<'k>(
        &self,
        ty: &dyn FilterType,
        pairs: impl IntoIterator<Item = (&'k str, Value)>,
    ) -> Result<Self, FilterError> {
        let mut out = *self;
        let fields = ty.fields();
        for (key, value) in pairs {
            let Some(index) = fields.iter().position(|f| f.key == key) else {
                return Err(FilterError::UnknownKey(key.to_owned()));
            };
            let field = &fields[index];
            match (field.kind, value) {
                (FieldKind::Number { min, max, .. }, Value::Number(v)) => {
                    if !v.is_finite() || v < min || v > max {
                        return Err(FilterError::OutOfRange(field.key));
                    }
                }
                (FieldKind::Color { .. }, Value::Color(_)) => {}
                _ => return Err(FilterError::WrongKind(field.key)),
            }
            if index < MAX_FIELDS {
                out.items[index] = value;
            }
        }
        Ok(out)
    }

    /// The values in field order.
    pub fn as_slice(&self) -> &[Value] {
        &self.items[..self.len]
    }

    /// Value `index`, if the type has that many fields.
    pub fn get(&self, index: usize) -> Option<Value> {
        self.as_slice().get(index).copied()
    }

    /// Number `index`, or 0.0 when it is not a number.
    pub fn number(&self, index: usize) -> f32 {
        match self.get(index) {
            Some(Value::Number(v)) => v,
            _ => 0.0,
        }
    }

    /// Colour `index`, or transparent black when it is not a colour.
    pub fn color(&self, index: usize) -> [u8; 4] {
        match self.get(index) {
            Some(Value::Color(c)) => c,
            _ => [0; 4],
        }
    }

    /// `(key, value)` for every field, for saving.
    pub fn to_pairs(&self, ty: &dyn FilterType) -> Vec<(&'static str, Value)> {
        ty.fields()
            .iter()
            .zip(self.as_slice())
            .map(|(f, v)| (f.key, *v))
            .collect()
    }
}

/// `#rrggbb` for an opaque colour, else `#rrggbbaa`.
pub fn color_text(c: [u8; 4]) -> String {
    if c[3] == 255 {
        format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", c[0], c[1], c[2], c[3])
    }
}

/// `#rrggbb` or `#rrggbbaa` (either case) as a colour.
pub fn parse_color(text: &str) -> Option<[u8; 4]> {
    let hex = text.strip_prefix('#')?;
    if !(hex.len() == 6 || hex.len() == 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    Some([
        byte(0)?,
        byte(2)?,
        byte(4)?,
        if hex.len() == 8 { byte(6)? } else { 255 },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_round_trip_as_text() {
        assert_eq!(parse_color("#00ff00"), Some([0, 255, 0, 255]));
        assert_eq!(parse_color("#00FF0080"), Some([0, 255, 0, 128]));
        assert_eq!(parse_color("00ff00"), None);
        assert_eq!(parse_color("#0f0"), None);
        assert_eq!(parse_color("#gg0000"), None);
        assert_eq!(color_text([0, 255, 0, 255]), "#00ff00");
        assert_eq!(color_text([1, 2, 3, 4]), "#01020304");
    }

    #[test]
    fn half_scale_rounds_up() {
        assert_eq!(Scale::Half.of([1921, 1081]), [961, 541]);
        assert_eq!(Scale::Half.of([1, 1]), [1, 1]);
        assert_eq!(Scale::Full.of([7, 3]), [7, 3]);
    }
}
