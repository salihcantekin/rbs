//! What each built-in source type supports for its activity policy (ADR-0152
//! decisions 4 and 5): when it runs and what it does when it comes back.

use rbs_graph::node::NodeTypeId;
use rbs_scene::activity::{ActivityTraits, OnReturn, Run};

use crate::types::{SourceClass, class_of};

/// What a built-in source type supports for its activity policy (ADR-0152); any
/// other type, plugins included, is [`ActivityTraits::PLACED`]. The same answer as
/// [`NodeTypeTable`]'s `NodeTypes::activity`, for the host's checks.
pub fn activity_traits(type_id: &NodeTypeId) -> ActivityTraits {
    match class_of(type_id) {
        Some(SourceClass::Media) => MEDIA_ACTIVITY,
        Some(SourceClass::Browser) => BROWSER_ACTIVITY,
        Some(SourceClass::Capture(_)) => CAPTURE_ACTIVITY,
        Some(SourceClass::Slideshow) => SLIDESHOW_ACTIVITY,
        _ => ActivityTraits::PLACED,
    }
}

/// A display, window or camera (ADR-0152 decision 5, phase 4): open while placed by
/// default, so a cut to it is never black and an exclusive device stays held. With
/// `shown` or `on_air` the session stops below that level and opens again on the
/// capture node's own thread when the source comes back (the layer is empty for the
/// warm-up). No `on_return`: a live picture has no position. Linked while warm, as
/// today.
pub const CAPTURE_ACTIVITY: ActivityTraits = ActivityTraits::new(
    &[Run::Placed, Run::Shown, Run::OnAir],
    &[],
    Run::Placed,
    None,
    true,
);

/// A media file (ADR-0152 decision 4, ADR-0108): Active while a shown scene draws
/// it, starting over when it goes on air again; `continue` picks up where it would
/// be had it kept playing (phase 5).
pub const MEDIA_ACTIVITY: ActivityTraits = ActivityTraits::new(
    &[Run::Placed, Run::Shown, Run::OnAir],
    &[OnReturn::Resume, OnReturn::Restart, OnReturn::Continue],
    Run::Shown,
    Some(OnReturn::Restart),
    false,
);

/// The return edge a media file's control handle takes for `on_return` (ADR-0152
/// decisions 6 and 7).
pub fn media_return_edge(on_return: OnReturn) -> rbs_nodes::MediaReturnEdge {
    match on_return {
        OnReturn::Restart => rbs_nodes::MediaReturnEdge::Restart,
        OnReturn::Continue => rbs_nodes::MediaReturnEdge::Continue,
        _ => rbs_nodes::MediaReturnEdge::Resume,
    }
}

/// A slide show (ADR-0171 decision 10): Active while a shown scene draws it; on return
/// it continues with the picture it showed (the index survives Standby, no pixels do).
pub const SLIDESHOW_ACTIVITY: ActivityTraits = ActivityTraits::new(
    &[Run::Placed, Run::Shown, Run::OnAir],
    &[OnReturn::Resume],
    Run::Shown,
    Some(OnReturn::Resume),
    false,
);

/// A browser page (ADR-0152 decision 4, ADR-0109): Active while a shown scene draws
/// it; `run: placed` is ADR-0149's background page (process running, picture node
/// unscheduled, one picture per second, sound dropped). `on_return: restart`
/// reloads a running page when it goes on air (decision 6).
pub const BROWSER_ACTIVITY: ActivityTraits = ActivityTraits::new(
    &[Run::Placed, Run::Shown, Run::OnAir],
    &[OnReturn::Continue, OnReturn::Restart],
    Run::Shown,
    Some(OnReturn::Continue),
    false,
);

#[cfg(test)]
mod activity_tests {
    use super::*;
    use crate::registry::NodeTypeTable;
    use crate::types;
    use rbs_graph::node::NodeTypeId;
    use rbs_scene::binder::NodeTypes;

    /// Every built-in type declares a default it allows, and only media, browser and
    /// slide show leave the placed rule by default (ADR-0152 decisions 4 and 5,
    /// ADR-0171 decision 10); captures may.
    #[test]
    fn built_in_activity_defaults_are_allowed_and_only_timed_types_are_temporal() {
        let table = NodeTypeTable::default();
        for name in types::ALL {
            let traits = table.activity(&NodeTypeId::new(name));
            assert!(traits.runs.contains(&traits.default_run), "{name}");
            assert_eq!(
                traits.default_on_return.is_some(),
                !traits.on_returns.is_empty(),
                "{name}"
            );
            assert!(
                traits
                    .default_on_return
                    .is_none_or(|o| traits.on_returns.contains(&o)),
                "{name}"
            );
            let temporal = traits.default_run != Run::Placed;
            let expected = matches!(
                name,
                types::MEDIA_SOURCE | types::BROWSER_SOURCE | types::SLIDESHOW_SOURCE
            );
            assert_eq!(temporal, expected, "{name}");
            let capture = matches!(
                name,
                types::DISPLAY_SOURCE | types::WINDOW_SOURCE | types::CAMERA_SOURCE
            );
            assert_eq!(
                traits.runs.contains(&Run::OnAir),
                expected || capture,
                "{name}"
            );
        }
        assert_eq!(
            table.activity(&NodeTypeId::new("org.example.plugin")),
            ActivityTraits::PLACED
        );
    }

    /// A film offers `continue` (phase 5), and each `on_return` reaches its handle
    /// as its own return edge.
    #[test]
    fn a_film_continues_and_each_return_reaches_its_handle() {
        use rbs_nodes::MediaReturnEdge as Edge;
        assert!(MEDIA_ACTIVITY.on_returns.contains(&OnReturn::Continue));
        assert_eq!(media_return_edge(OnReturn::Restart), Edge::Restart);
        assert_eq!(media_return_edge(OnReturn::Resume), Edge::Resume);
        assert_eq!(media_return_edge(OnReturn::Continue), Edge::Continue);
    }
}
