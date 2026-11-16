//! Capability keys of the text rendering port (ADR-0129 decision 2).

use rbs_core::capability::CapabilityKey;

/// Flag: draws colour glyphs (emoji) in colour. The host prefers a renderer with it
/// when the text holds emoji.
pub const COLOR_GLYPHS: CapabilityKey = CapabilityKey::new_static("text.color_glyphs");
/// Flag: honours [`crate::TextBlock::tabular_figures`] when the font has them.
pub const TABULAR_FIGURES: CapabilityKey = CapabilityKey::new_static("text.tabular_figures");
/// Flag: lists installed families from the system (false for a renderer that only
/// knows fonts it was given).
pub const SYSTEM_FONTS: CapabilityKey = CapabilityKey::new_static("text.system_fonts");
