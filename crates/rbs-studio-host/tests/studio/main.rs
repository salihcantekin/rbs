//! The studio host's integration tests, one binary (ADR-0159 decision 7): each module
//! was its own `rbs-app` test file and keeps its own helpers.

mod add_ons;
mod api;
mod attributes;
mod chapters;
mod command_path;
mod faults;
mod filters;
mod host;
mod loudness;
mod monitor;
mod projector;
mod record;
mod replay;
mod scenes;
mod stream;
mod sync_offset;
mod vcam;
