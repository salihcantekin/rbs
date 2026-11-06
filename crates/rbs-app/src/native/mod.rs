//! Native compositor layers of the UI and the app's quit route (macOS). Marked
//! low-level module (P11): calls into Core Animation and AppKit, each `unsafe` block
//! with its invariant.

mod meter_layer;
mod quit;

pub use meter_layer::{MeterBridgeLayer, MeterColors, MeterGeometry};
pub use quit::route_quit;
