//! Keeping RBS running without a window on Windows (ADR-0056): the tray icon and the
//! named event a second launch signals. Marked low-level module (P11): calls into
//! Win32, each `unsafe` block with its invariant.

mod show_event;
mod tray;

pub use show_event::{ShowEvent, signal_running};
pub use tray::{Tray, install_handlers};
