//! The settings schema shared by every kind of filter (ADR-0082 decision 2).
//!
//! A filter type describes its settings as a list of [`Field`]s. [`Values`] holds one
//! valid [`Value`] per field, in field order, in a fixed-size array, so it moves
//! through a parameter channel without allocating (ADR-0023). The UI, the control API
//! and the scene document are all generated from the schema, so a new filter needs no
//! per-filter UI or persistence code.
//!
//! The crate has no dependencies: video filters (GPU) and audio filters (DSP) share it
//! without either layer depending on the other (P1).

use std::fmt;

/// Most fields a filter type may have. A video filter type has fewer (its own limit is
/// checked by its conformance suite).
pub const MAX_FIELDS: usize = 12;

/// Where a number field's control puts its values.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Curve {
    /// Evenly spaced.
    #[default]
    Linear,
    /// Evenly spaced in the logarithm: for times and frequencies that span decades.
    /// Needs `min > 0`.
    Log,
}

/// One setting of a filter type. Build with the constructors, which stay compatible
/// when fields are added.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Field {
    /// Settings key: lowercase ASCII letters, digits and `_`.
    pub key: &'static str,
    /// English label for the UI.
    pub label: &'static str,
    /// What it holds.
    pub kind: FieldKind,
    /// One plain sentence the UI shows with the control; may be empty.
    pub help: &'static str,
}

impl Field {
    /// A number in `min..=max`; `step` is the UI's increment and `unit` is shown after
    /// the value ("px", "dB"; may be empty). Linear; see [`Field::log`].
    pub const fn number(
        key: &'static str,
        label: &'static str,
        min: f32,
        max: f32,
        default: f32,
        step: f32,
        unit: &'static str,
    ) -> Self {
        Self {
            key,
            label,
            kind: FieldKind::Number {
                min,
                max,
                default,
                step,
                unit,
                curve: Curve::Linear,
            },
            help: "",
        }
    }

    /// An sRGB colour with straight alpha.
    pub const fn color(key: &'static str, label: &'static str, default: [u8; 4]) -> Self {
        Self {
            key,
            label,
            kind: FieldKind::Color { default },
            help: "",
        }
    }

    /// An on/off switch.
    pub const fn toggle(key: &'static str, label: &'static str, default: bool) -> Self {
        Self {
            key,
            label,
            kind: FieldKind::Toggle { default },
            help: "",
        }
    }

    /// One of `options` (`(key, label)` pairs, at most [`MAX_OPTIONS`]); `default` is
    /// the key of one of them.
    pub const fn choice(
        key: &'static str,
        label: &'static str,
        options: &'static [(&'static str, &'static str)],
        default: &'static str,
    ) -> Self {
        Self {
            key,
            label,
            kind: FieldKind::Choice { options, default },
            help: "",
        }
    }

    /// Another source of the collection, or none (an audio source for a ducker's key,
    /// ADR-0095). The default is none.
    pub const fn source(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: FieldKind::Source,
            help: "",
        }
    }

    /// This field with `help` text.
    pub const fn with_help(mut self, help: &'static str) -> Self {
        self.help = help;
        self
    }

    /// This number field on a logarithmic scale; other kinds are unchanged. A field
    /// whose `min` is not above 0 fails the conformance suite.
    pub const fn log(mut self) -> Self {
        if let FieldKind::Number { curve, .. } = &mut self.kind {
            *curve = Curve::Log;
        }
        self
    }
}

/// Most options a [`FieldKind::Choice`] may have.
pub const MAX_OPTIONS: usize = 255;

/// The kind of a field and its range or default.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldKind {
    /// A number in `min..=max`.
    #[non_exhaustive]
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
        /// How the control spaces the values.
        curve: Curve,
    },
    /// An sRGB colour with straight alpha.
    Color {
        /// Value when not set.
        default: [u8; 4],
    },
    /// An on/off switch.
    Toggle {
        /// Value when not set.
        default: bool,
    },
    /// One of a fixed list.
    Choice {
        /// `(key, label)` of each option, in display order. The key is what is saved.
        options: &'static [(&'static str, &'static str)],
        /// The key of the option used when not set.
        default: &'static str,
    },
    /// A reference to another source by id; the default is none.
    Source,
}

impl FieldKind {
    /// The default value. A choice whose default key is not among its options gives
    /// the first option (the conformance suite refuses such a schema).
    pub fn default_value(&self) -> Value {
        match *self {
            FieldKind::Number { default, .. } => Value::Number(default),
            FieldKind::Color { default } => Value::Color(default),
            FieldKind::Toggle { default } => Value::Toggle(default),
            FieldKind::Choice { options, default } => Value::Choice(
                options
                    .iter()
                    .position(|(key, _)| *key == default)
                    .and_then(|i| u8::try_from(i).ok())
                    .unwrap_or(0),
            ),
            FieldKind::Source => Value::Source(None),
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
    /// An on/off switch.
    Toggle(bool),
    /// An index into the field's options.
    Choice(u8),
    /// A source id, or none.
    Source(Option<u32>),
}

/// Why values were refused.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamError {
    /// The type has no field with this key.
    UnknownKey(String),
    /// A value of the wrong kind for the field.
    WrongKind(&'static str),
    /// A number outside its field's range, or not finite.
    OutOfRange(&'static str),
    /// A choice that is not one of the field's options.
    UnknownChoice(&'static str),
}

impl fmt::Display for ParamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParamError::UnknownKey(k) => write!(f, "no setting named {k:?}"),
            ParamError::WrongKind(k) => write!(f, "setting {k:?} has the wrong kind of value"),
            ParamError::OutOfRange(k) => write!(f, "setting {k:?} is out of range"),
            ParamError::UnknownChoice(k) => write!(f, "setting {k:?} has no such option"),
        }
    }
}

impl std::error::Error for ParamError {}

/// Anything with a settings schema: a filter type of any kind, or a bare field list.
pub trait Schema {
    /// The fields, in display order.
    fn fields(&self) -> &'static [Field];
}

impl Schema for &'static [Field] {
    fn fields(&self) -> &'static [Field] {
        self
    }
}

/// One valid value per field of a schema, in field order. Fixed size, so it moves
/// through a parameter channel without allocating (ADR-0023).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Values {
    len: usize,
    items: [Value; MAX_FIELDS],
}

impl Values {
    /// Every field's default.
    pub fn defaults<S: Schema + ?Sized>(schema: &S) -> Self {
        let mut items = [Value::Number(0.0); MAX_FIELDS];
        let fields = schema.fields();
        for (slot, field) in items.iter_mut().zip(fields) {
            *slot = field.kind.default_value();
        }
        Self {
            len: fields.len().min(MAX_FIELDS),
            items,
        }
    }

    /// The defaults with the given keys replaced; refused as a whole if any pair is.
    pub fn from_pairs<'k, S: Schema + ?Sized>(
        schema: &S,
        pairs: impl IntoIterator<Item = (&'k str, Value)>,
    ) -> Result<Self, ParamError> {
        Self::defaults(schema).with(schema, pairs)
    }

    /// These values with the given keys replaced (a patch); refused as a whole if any
    /// pair is.
    pub fn with<'k, S: Schema + ?Sized>(
        &self,
        schema: &S,
        pairs: impl IntoIterator<Item = (&'k str, Value)>,
    ) -> Result<Self, ParamError> {
        let mut out = *self;
        let fields = schema.fields();
        for (key, value) in pairs {
            let Some(index) = fields.iter().position(|f| f.key == key) else {
                return Err(ParamError::UnknownKey(key.to_owned()));
            };
            let field = &fields[index];
            match (field.kind, value) {
                (FieldKind::Number { min, max, .. }, Value::Number(v)) => {
                    if !v.is_finite() || v < min || v > max {
                        return Err(ParamError::OutOfRange(field.key));
                    }
                }
                (FieldKind::Color { .. }, Value::Color(_))
                | (FieldKind::Toggle { .. }, Value::Toggle(_))
                | (FieldKind::Source, Value::Source(_)) => {}
                (FieldKind::Choice { options, .. }, Value::Choice(i)) => {
                    if usize::from(i) >= options.len() {
                        return Err(ParamError::UnknownChoice(field.key));
                    }
                }
                _ => return Err(ParamError::WrongKind(field.key)),
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

    /// Value `index`, if the schema has that many fields.
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

    /// Switch `index`, or `false` when it is not a switch.
    pub fn toggle(&self, index: usize) -> bool {
        matches!(self.get(index), Some(Value::Toggle(true)))
    }

    /// Choice `index` (an option index), or 0 when it is not a choice.
    pub fn choice(&self, index: usize) -> u8 {
        match self.get(index) {
            Some(Value::Choice(i)) => i,
            _ => 0,
        }
    }

    /// Source `index` (a source id), or `None` when none is set or it is not a source.
    pub fn source(&self, index: usize) -> Option<u32> {
        match self.get(index) {
            Some(Value::Source(id)) => id,
            _ => None,
        }
    }

    /// `(key, value)` for every field, for saving.
    pub fn to_pairs<S: Schema + ?Sized>(&self, schema: &S) -> Vec<(&'static str, Value)> {
        schema
            .fields()
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

/// Checks a schema and returns what is wrong with it, for the conformance suites of
/// the filter kinds: key charset and uniqueness, number ranges and defaults, `Log`
/// only above 0, a choice default among its options, the option count and the field
/// count (at most `max_fields`).
pub fn check_fields(fields: &[Field], max_fields: usize) -> Result<(), String> {
    if fields.len() > max_fields.min(MAX_FIELDS) {
        return Err(format!("{} fields; at most {max_fields}", fields.len()));
    }
    for (i, f) in fields.iter().enumerate() {
        let key_ok = !f.key.is_empty()
            && f.key
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !key_ok {
            return Err(format!(
                "field key {:?} is not lowercase ASCII, digits and _",
                f.key
            ));
        }
        if fields[..i].iter().any(|g| g.key == f.key) {
            return Err(format!("field key {:?} is used twice", f.key));
        }
        match f.kind {
            FieldKind::Number {
                min,
                max,
                default,
                step,
                curve,
                ..
            } => {
                let finite = min.is_finite() && max.is_finite() && default.is_finite();
                if !finite || min >= max || default < min || default > max {
                    return Err(format!("{}: range or default is not valid", f.key));
                }
                if !(step.is_finite() && step > 0.0) {
                    return Err(format!("{}: step must be above 0", f.key));
                }
                if curve == Curve::Log && min <= 0.0 {
                    return Err(format!("{}: a log field needs min above 0", f.key));
                }
            }
            FieldKind::Choice { options, default } => {
                if options.is_empty() || options.len() > MAX_OPTIONS {
                    return Err(format!("{}: 1 to {MAX_OPTIONS} options", f.key));
                }
                if !options.iter().any(|(k, _)| *k == default) {
                    return Err(format!("{}: default is not one of the options", f.key));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPTIONS: &[(&str, &str)] = &[("fast", "Fast"), ("slow", "Slow")];
    const FIELDS: &[Field] = &[
        Field::number("gain", "Gain", -30.0, 30.0, 0.0, 0.5, "dB"),
        Field::number("release", "Release", 5.0, 1000.0, 150.0, 1.0, "ms").log(),
        Field::toggle("auto", "Automatic", true),
        Field::choice("mode", "Mode", OPTIONS, "slow"),
        Field::color("tint", "Tint", [0, 255, 0, 255]),
    ];

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
    fn defaults_cover_every_kind() {
        let v = Values::defaults(&FIELDS);
        assert_eq!(v.number(0), 0.0);
        assert_eq!(v.number(1), 150.0);
        assert!(v.toggle(2));
        assert_eq!(v.choice(3), 1);
        assert_eq!(v.color(4), [0, 255, 0, 255]);
        assert_eq!(v.as_slice().len(), FIELDS.len());
    }

    #[test]
    fn a_patch_replaces_keys_and_is_refused_as_a_whole() {
        let v = Values::defaults(&FIELDS);
        let w = v
            .with(
                &FIELDS,
                [
                    ("gain", Value::Number(6.0)),
                    ("auto", Value::Toggle(false)),
                    ("mode", Value::Choice(0)),
                ],
            )
            .unwrap_or(v);
        assert_eq!(w.number(0), 6.0);
        assert!(!w.toggle(2));
        assert_eq!(w.choice(3), 0);
        assert_eq!(
            v.with(
                &FIELDS,
                [("gain", Value::Number(6.0)), ("nope", Value::Number(1.0))]
            ),
            Err(ParamError::UnknownKey("nope".to_owned()))
        );
        assert_eq!(
            v.with(&FIELDS, [("gain", Value::Number(31.0))]),
            Err(ParamError::OutOfRange("gain"))
        );
        assert_eq!(
            v.with(&FIELDS, [("gain", Value::Number(f32::NAN))]),
            Err(ParamError::OutOfRange("gain"))
        );
        assert_eq!(
            v.with(&FIELDS, [("gain", Value::Toggle(true))]),
            Err(ParamError::WrongKind("gain"))
        );
        assert_eq!(
            v.with(&FIELDS, [("mode", Value::Choice(2))]),
            Err(ParamError::UnknownChoice("mode"))
        );
    }

    #[test]
    fn a_source_field_defaults_to_none_and_takes_an_id() {
        const SRC: &[Field] = &[Field::source("key_source", "Key source")];
        let v = Values::defaults(&SRC);
        assert_eq!(v.source(0), None);
        let w = v
            .with(&SRC, [("key_source", Value::Source(Some(0)))])
            .unwrap_or(v);
        assert_eq!(w.source(0), Some(0));
        assert_eq!(
            w.with(&SRC, [("key_source", Value::Toggle(true))]),
            Err(ParamError::WrongKind("key_source"))
        );
        assert_eq!(check_fields(SRC, MAX_FIELDS), Ok(()));
    }

    #[test]
    fn pairs_round_trip() {
        let v = Values::defaults(&FIELDS);
        let pairs = v.to_pairs(&FIELDS);
        assert_eq!(pairs.len(), FIELDS.len());
        assert_eq!(Values::from_pairs(&FIELDS, pairs.iter().copied()), Ok(v));
    }

    #[test]
    fn a_valid_schema_passes_the_check() {
        assert_eq!(check_fields(FIELDS, MAX_FIELDS), Ok(()));
        assert!(check_fields(FIELDS, 4).is_err());
    }

    #[test]
    fn bad_schemas_are_found() {
        let bad = |f: &[Field]| check_fields(f, MAX_FIELDS).is_err();
        assert!(bad(&[Field::number("a", "A", 1.0, 1.0, 1.0, 1.0, "")]));
        assert!(bad(&[Field::number("a", "A", 0.0, 1.0, 2.0, 1.0, "")]));
        assert!(bad(&[Field::number("a", "A", 0.0, 1.0, 0.5, 0.0, "")]));
        assert!(bad(
            &[Field::number("a", "A", 0.0, 1.0, 0.5, 0.1, "").log()]
        ));
        assert!(bad(&[Field::number("A", "A", 0.0, 1.0, 0.5, 0.1, "")]));
        assert!(bad(&[
            Field::toggle("a", "A", true),
            Field::toggle("a", "B", true)
        ]));
        assert!(bad(&[Field::choice("a", "A", OPTIONS, "medium")]));
        assert!(bad(&[Field::choice("a", "A", &[], "x")]));
    }
}
