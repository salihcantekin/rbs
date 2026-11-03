//! Pure helpers that turn what the PipeWire registry says into port types: endpoint
//! ids, channel layouts and metadata values. No PipeWire calls, so they are tested on
//! every platform.

use rbs_audio_io::{EndpointId, EndpointKind};
use rbs_core::media::{ChannelLayout, MAX_CHANNELS};

/// Id prefix of `Input` endpoints: an `Audio/Source` node, by `node.name`.
pub(crate) const SOURCE_PREFIX: &str = "source:";
/// Id prefix of `OutputLoopback` endpoints: the monitor of an `Audio/Sink` node, by
/// `node.name`.
pub(crate) const MONITOR_PREFIX: &str = "monitor:";
/// Id prefix of `Application` endpoints: every `Stream/Output/Audio` node of one
/// application, by `application.name`.
pub(crate) const APP_PREFIX: &str = "app:";

/// The endpoint id of `name` for `kind`.
pub(crate) fn endpoint_id(kind: EndpointKind, name: &str) -> EndpointId {
    let prefix = match kind {
        EndpointKind::OutputLoopback => MONITOR_PREFIX,
        EndpointKind::Application => APP_PREFIX,
        _ => SOURCE_PREFIX,
    };
    EndpointId::new(format!("{prefix}{name}"))
}

/// The kind and node or application name an endpoint id names.
pub(crate) fn parse_id(id: &EndpointId) -> Option<(EndpointKind, &str)> {
    let id = id.as_str();
    let (kind, name) = if let Some(name) = id.strip_prefix(SOURCE_PREFIX) {
        (EndpointKind::Input, name)
    } else if let Some(name) = id.strip_prefix(MONITOR_PREFIX) {
        (EndpointKind::OutputLoopback, name)
    } else {
        (EndpointKind::Application, id.strip_prefix(APP_PREFIX)?)
    };
    (!name.is_empty()).then_some((kind, name))
}

/// The media class an endpoint kind reads from.
pub(crate) fn class_matches(kind: EndpointKind, class: &str) -> bool {
    match kind {
        // Duplex nodes (the Pro Audio profile of multi-channel interfaces) capture
        // through their output ports too.
        EndpointKind::Input => class.starts_with("Audio/Source") || class == "Audio/Duplex",
        EndpointKind::OutputLoopback => class.starts_with("Audio/Sink"),
        EndpointKind::Application => class == "Stream/Output/Audio",
        _ => false,
    }
}

/// Layouts with speaker positions, in the planar order of `ChannelLayout`, with the
/// PipeWire position names each slot accepts.
const NAMED: [(ChannelLayout, &[&[&str]]); 5] = [
    (ChannelLayout::Mono, &[&["MONO", "FC", "FL"]]),
    (ChannelLayout::Stereo, &[&["FL"], &["FR"]]),
    (
        ChannelLayout::Surround3_1,
        &[&["FL"], &["FR"], &["FC"], &["LFE"]],
    ),
    (
        ChannelLayout::Surround5_1,
        &[
            &["FL"],
            &["FR"],
            &["FC"],
            &["LFE"],
            &["SL", "RL"],
            &["SR", "RR"],
        ],
    ),
    (
        ChannelLayout::Surround7_1,
        &[
            &["FL"],
            &["FR"],
            &["FC"],
            &["LFE"],
            &["SL"],
            &["SR"],
            &["RL"],
            &["RR"],
        ],
    ),
];

/// The layout of a node whose output ports carry `channels` (PipeWire position names,
/// in port order), and the positions to ask for, in the layout's planar order.
///
/// Known speaker sets map to their layout whatever the port order (ALSA lists 5.1 as
/// FL FR RL RR FC LFE). Anything else is `Discrete` in port order, cut to the first
/// [`MAX_CHANNELS`]. No ports at all reads as stereo, the usual default.
pub(crate) fn layout_of(channels: &[String]) -> (ChannelLayout, Vec<String>) {
    if channels.is_empty() {
        return (ChannelLayout::Stereo, vec!["FL".into(), "FR".into()]);
    }
    for (layout, slots) in NAMED {
        if slots.len() != channels.len() {
            continue;
        }
        let mut positions = Vec::with_capacity(slots.len());
        for names in slots {
            match channels.iter().find(|c| names.contains(&c.as_str())) {
                Some(found) if !positions.contains(found) => positions.push(found.clone()),
                _ => break,
            }
        }
        if positions.len() == slots.len() {
            return (layout, positions);
        }
    }
    let positions: Vec<String> = channels.iter().take(MAX_CHANNELS).cloned().collect();
    // At most MAX_CHANNELS (8), so the cast cannot truncate.
    (ChannelLayout::Discrete(positions.len() as u8), positions)
}

/// The `name` of a `default` metadata value such as `{ "name": "alsa_input.usb" }`.
pub(crate) fn default_name(value: &str) -> Option<&str> {
    let rest = &value[value.find("\"name\"")? + "\"name\"".len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Which of an application's ports feed which of the capture stream's stereo ports:
/// left positions into `FL`, right positions into `FR`, centre and mono positions into
/// both, LFE into neither, and unknown positions (`AUXn`) by port index. PipeWire's port
/// mixer sums what is linked at unity gain, so a surround application is downmixed
/// without losing its centre (dialogue) channel.
pub(crate) fn feeds(app_channel: &str, app_index: u32, ours: &str, our_index: u32) -> bool {
    const LEFT: [&str; 9] = ["FL", "SL", "RL", "FLC", "RLC", "FLW", "FLH", "TFL", "TRL"];
    const RIGHT: [&str; 9] = ["FR", "SR", "RR", "FRC", "RRC", "FRW", "FRH", "TFR", "TRR"];
    const BOTH: [&str; 8] = ["MONO", "FC", "RC", "BC", "TC", "TFC", "TRC", "FCH"];
    if LEFT.contains(&app_channel) {
        ours == "FL"
    } else if RIGHT.contains(&app_channel) {
        ours == "FR"
    } else if BOTH.contains(&app_channel) {
        true
    } else if app_channel.starts_with("LFE") {
        false
    } else {
        app_index == our_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn ids_round_trip() {
        for kind in EndpointKind::ALL {
            let id = endpoint_id(kind, "alsa_input.usb-mic");
            assert_eq!(parse_id(&id), Some((kind, "alsa_input.usb-mic")));
        }
        assert_eq!(parse_id(&EndpointId::new("source:")), None);
        assert_eq!(parse_id(&EndpointId::new("node:x")), None);
    }

    #[test]
    fn classes_select_kinds() {
        assert!(class_matches(EndpointKind::Input, "Audio/Source"));
        assert!(class_matches(EndpointKind::Input, "Audio/Source/Virtual"));
        assert!(class_matches(EndpointKind::Input, "Audio/Duplex"));
        assert!(class_matches(EndpointKind::OutputLoopback, "Audio/Sink"));
        assert!(!class_matches(EndpointKind::Input, "Stream/Input/Audio"));
        assert!(class_matches(
            EndpointKind::Application,
            "Stream/Output/Audio"
        ));
    }

    #[test]
    fn layouts_follow_speaker_sets() {
        assert_eq!(
            layout_of(&names(&["MONO"])),
            (ChannelLayout::Mono, names(&["MONO"]))
        );
        assert_eq!(
            layout_of(&names(&["FR", "FL"])),
            (ChannelLayout::Stereo, names(&["FL", "FR"]))
        );
        assert_eq!(
            layout_of(&names(&["FL", "FR", "RL", "RR", "FC", "LFE"])),
            (
                ChannelLayout::Surround5_1,
                names(&["FL", "FR", "FC", "LFE", "RL", "RR"])
            )
        );
        assert_eq!(
            layout_of(&names(&["AUX0", "AUX1"])),
            (ChannelLayout::Discrete(2), names(&["AUX0", "AUX1"]))
        );
        let sixteen: Vec<String> = (0..16).map(|i| format!("AUX{i}")).collect();
        let (layout, positions) = layout_of(&sixteen);
        assert_eq!(layout, ChannelLayout::Discrete(8));
        assert_eq!(positions, sixteen[..8]);
        assert_eq!(layout_of(&[]).0, ChannelLayout::Stereo);
        // Two ports with the same position are not stereo.
        assert_eq!(
            layout_of(&names(&["FL", "FL"])).0,
            ChannelLayout::Discrete(2)
        );
    }

    #[test]
    fn default_names_parse() {
        assert_eq!(
            default_name(r#"{"name":"alsa_input.pci"}"#),
            Some("alsa_input.pci")
        );
        assert_eq!(default_name(r#"{ "name" : "x" }"#), Some("x"));
        assert_eq!(default_name("{}"), None);
    }

    #[test]
    fn application_ports_feed_stereo() {
        assert!(feeds("FL", 0, "FL", 0));
        assert!(!feeds("FL", 0, "FR", 1));
        assert!(feeds("MONO", 0, "FR", 1));
        assert!(feeds("FC", 2, "FL", 0) && feeds("FC", 2, "FR", 1));
        assert!(feeds("RR", 5, "FR", 1) && !feeds("RR", 5, "FL", 0));
        assert!(!feeds("LFE", 3, "FL", 0));
        assert!(feeds("AUX1", 1, "FR", 1));
        assert!(!feeds("AUX2", 2, "FR", 1));
    }
}
