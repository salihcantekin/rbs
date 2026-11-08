//! Native compositor layers of the UI, the app's quit route and its way back from the
//! background (macOS). Marked low-level module (P11): calls into Core Animation and
//! AppKit, each `unsafe` block with its invariant.

mod background;
mod meter_layer;
mod quit;

pub use background::{
    request_attention, route_show, set_app_name, set_badge, set_hidden, set_icon,
};
pub use meter_layer::{MeterBridgeLayer, MeterColors, MeterGeometry};
pub use quit::route_quit;
