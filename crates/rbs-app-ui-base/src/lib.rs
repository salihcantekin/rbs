//! The shared globals, structs and enums of the desktop UI (`crates/rbs-app/ui/base`),
//! generated as the Slint library `rbsui` (ADR-0138). Only the facade `rbs-app-ui` names
//! this crate.
#![allow(missing_docs, clippy::all, clippy::unwrap_used, clippy::expect_used)]

#[cfg(not(rbs_ui_skip))]
slint::include_modules!();
