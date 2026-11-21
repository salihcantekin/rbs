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

/// `item.set_style` as the window sends it (ADR-0164).
fn set_style(
    item: u32,
    style: &rbs_scene::model::ItemStyle,
    canvas: rbs_studio_host::studio_host::OutputCanvas,
) -> rbs_studio_host::studio_host::Command {
    rbs_studio_host::studio_host::Command::Api {
        call: Box::new(rbs_api::Command::ItemSetStyle(
            rbs_api::request::SetItemStyle {
                item,
                style: rbs_studio_host::studio_host::style_for_api(style),
                canvas,
                gesture: 0,
            },
        )),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    }
}

/// `item.set_scroll` as the window sends it (ADR-0164).
fn set_scroll(
    item: u32,
    scroll: Option<rbs_scene::model::Scroll>,
    canvas: rbs_studio_host::studio_host::OutputCanvas,
) -> rbs_studio_host::studio_host::Command {
    rbs_studio_host::studio_host::Command::Api {
        call: Box::new(rbs_api::Command::ItemSetScroll(
            rbs_api::request::SetItemScroll {
                item,
                scroll: scroll
                    .as_ref()
                    .map(rbs_studio_host::studio_host::scroll_for_api),
                canvas,
                gesture: 0,
            },
        )),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    }
}
