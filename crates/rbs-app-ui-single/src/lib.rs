//! Every window and shared type of the desktop UI in one Slint compilation, used only by
//! the UI live preview (`cargo xtask ui-dev`, ADR-0138 decision 5). Only the facade
//! `rbs-app-ui` names this crate.
#![allow(missing_docs, clippy::all, clippy::unwrap_used, clippy::expect_used)]

#[cfg(not(rbs_ui_skip))]
slint::include_modules!();
