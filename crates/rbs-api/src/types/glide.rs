//! Slide timing of items between layouts (protocol 1.35, ADR-0100).

use serde::{Deserialize, Serialize};

/// How a slide maps unit time (protocol 1.35, ADR-0100). Fade-ins stay linear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum GlideEasing {
    /// Straight lerp.
    #[serde(rename = "linear")]
    Linear,
    /// Smoothstep ease in and out.
    #[serde(rename = "ease-in-out")]
    #[default]
    EaseInOut,
}

/// Collection or source glide timing (protocol 1.35, ADR-0100).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Glide {
    /// Milliseconds in `0..=5000`. `0` jumps.
    pub duration_ms: u32,
    /// Slide easing; fade-ins use the duration only.
    pub easing: GlideEasing,
}

impl Default for Glide {
    fn default() -> Self {
        Self {
            duration_ms: 400,
            easing: GlideEasing::EaseInOut,
        }
    }
}

impl Glide {
    /// True for the shipped default (400 ms, ease-in-out).
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}
