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

/// `source.set_settings` as the window sends it (ADR-0164): one undo step, or part of
/// `gesture`'s.
fn set_settings(
    id: rbs_scene::model::SourceId,
    settings: std::collections::BTreeMap<String, serde_json::Value>,
    gesture: u64,
) -> rbs_studio_host::studio_host::Command {
    rbs_studio_host::studio_host::Command::Api {
        call: Box::new(rbs_api::Command::SourceSetSettings(
            rbs_api::request::SetSourceSettings {
                source: id.get(),
                settings,
                gesture,
            },
        )),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    }
}
