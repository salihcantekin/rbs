//! Native compositor layers of the UI (macOS). Marked low-level module (P11): calls into
//! Core Animation, each `unsafe` block with its invariant.

mod meter_layer;

pub use meter_layer::{MeterBridgeLayer, MeterColors, MeterGeometry};
