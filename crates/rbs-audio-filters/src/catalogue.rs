//! The built-in audio filter types.

use crate::AudioFilterType;
use crate::gain::Gain;
use crate::gate::NoiseGate;

/// Every built-in audio filter type, in the order the UI lists them.
pub fn catalogue() -> &'static [&'static dyn AudioFilterType] {
    &[&Gain, &NoiseGate]
}

/// The built-in audio filter type with this id.
pub fn find(type_id: &str) -> Option<&'static dyn AudioFilterType> {
    catalogue()
        .iter()
        .copied()
        .find(|t| t.type_id().as_str() == type_id)
}
