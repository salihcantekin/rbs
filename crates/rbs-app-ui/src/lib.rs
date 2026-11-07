//! The Rust code Slint generates from `crates/rbs-app/ui/app.slint`, and the token
//! setters generated from the design token file (`build.rs`, rule U6).
//!
//! Only `rbs-app` uses this crate. It exists so the generated code, the largest part of
//! the app's build, is compiled once per build instead of once for the `rbs` binary and
//! again for its unit tests.
#![allow(missing_docs, clippy::all, clippy::unwrap_used, clippy::expect_used)]

slint::include_modules!();

/// Copies a token file's values into a window's `Theme` and `Tokens` globals
/// (`rbs-app` `look`, themes spec "Applying").
pub mod tokens_apply {
    use super::{Theme, ThemeSlot, Tokens};

    include!(concat!(env!("OUT_DIR"), "/tokens_apply.rs"));
}
