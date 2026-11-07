//! Windows-only parts of the app: keeping RBS running without a window (ADR-0056: the
//! tray icon and the named event a second launch signals) and the toolbar row as the
//! title bar (ADR-0071, [`caption`]). Marked low-level module (P11): calls into Win32,
//! each `unsafe` block with its invariant.

pub mod caption;
mod show_event;
mod tray;

pub use show_event::{ShowEvent, signal_running};
pub use tray::{Tray, install_handlers};
