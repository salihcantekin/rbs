//! Native compositor layers of the UI, the app's quit route and its way back from the
//! background (macOS). Marked low-level module (P11): calls into Core Animation and
//! AppKit, each `unsafe` block with its invariant.

mod background;
mod capture;
mod instance;
mod meter_layer;
mod quit;
pub mod titlebar;

pub use capture::{bring_forward, exclude_from_capture, forget_saved_frame};
pub use instance::reopen_running;

pub use background::{
    begin_quit_activity, request_attention, route_show, set_app_name, set_badge, set_hidden,
    set_icon,
};
pub use meter_layer::{MeterBridgeLayer, MeterColors, MeterGeometry};
pub use quit::route_quit;
