//! The icon and title an id shows: the entry's, or one that depends on a parameter.

use super::*;

/// The icon an id shows, from the design system's icon set: the entry's, or for an
/// area toggle the panel icon of its area.
pub fn icon(id: &CommandId, e: &Entry) -> Option<&'static str> {
    if let ("ui.layout.studio_rail", Some("below")) = (e.id, id.param("at")) {
        return Some("rail-below");
    }
    if let ("item.flip", Some("vertical")) = (e.id, id.param("axis")) {
        return Some("flip-v");
    }
    if let ("item.move" | "scene.move", Some(direction)) = (e.id, id.param("direction")) {
        return Some(match direction {
            "down" | "back" => "chevron-down",
            "front" => "export",
            _ => "chevron-up",
        });
    }
    if let ("ui.properties.show", Some(tab)) = (e.id, id.param("tab")) {
        return Some(match tab {
            "filters" => "sliders",
            "source" => "search",
            "audio" => "speaker",
            _ => "move",
        });
    }
    match (e.id, id.param("area")) {
        ("ui.area.toggle", Some("left")) => Some("panel-left"),
        ("ui.area.toggle", Some("right")) => Some("panel-right"),
        ("ui.area.toggle", Some("bottom")) => Some("panel-bottom"),
        _ => e.icon,
    }
}

/// The icon a picker's choice shows. Empty while the choice is the current one, so
/// the checkmark takes the icon's place (spec, "Icons").
pub fn choice_icon(id: &CommandId, checked: bool) -> Option<&'static str> {
    if checked {
        return None;
    }
    let name = match id.base() {
        "source.add" => match id.param("kind") {
            Some("screen") => "monitor",
            Some("window") => "window",
            Some("camera") => "camera",
            Some("audio_input") => "mic",
            Some("system_audio") | Some("application_audio") => "speaker",
            Some("media") | Some("image") => "image",
            Some("browser") => "window",
            Some("text") => "text",
            Some("existing") => "copy",
            _ => "plus",
        },
        "source.monitor" => match id.param("monitor") {
            Some("off") => "headphones-off",
            _ => "headphones",
        },
        "ui.mixer.orientation" => "layout",
        "ui.theme.select" => "palette",
        "ui.experience.select" => "sliders",
        "ui.density.select" => "layout",
        "ui.workspace.select" => "layout",
        "ui.panel.select" => "window",
        "ui.panel.move" => "move",
        "scene.select" => "image",
        "profile.select" => "sliders",
        "collection.select" => "folder",
        "item.rotate" => match id.param("degrees") {
            Some("-90") => "rotate-ccw",
            _ => "rotate-cw",
        },
        "item.fit" => "fit",
        "item.align" => "center",
        "item.flip" => match id.param("axis") {
            Some("vertical") => "flip-v",
            _ => "flip-h",
        },
        "item.reset_transform" => "reset",
        "filter.add" => "plus",
        _ => {
            return entry(id.base()).and_then(|e| icon(id, e));
        }
    };
    Some(name)
}

/// Whether a toolbar shows the id as its icon alone in every style: the area toggles
/// and Settings, whose icons say what they are (the title stays the accessible name
/// and tooltip).
pub fn icon_only(id: &CommandId) -> bool {
    matches!(
        id.base(),
        "ui.area.toggle" | "ui.settings.open" | "canvas.vertical_preview.toggle"
    )
}

/// The title an id shows: the entry's, with the area named for area toggles, the
/// panel for maximize and the position for `scene.position`.
pub fn title(id: &CommandId, e: &Entry) -> String {
    if let ("item.move", Some(direction)) = (e.id, id.param("direction")) {
        return match direction {
            "front" => "Bring to front".to_owned(),
            "up" => "Bring forward".to_owned(),
            "down" => "Send backward".to_owned(),
            "back" => "Send to back".to_owned(),
            _ => e.title.to_owned(),
        };
    }
    if let ("scene.move", Some(direction)) = (e.id, id.param("direction")) {
        return match direction {
            "front" => "Move to top".to_owned(),
            "up" => "Move up".to_owned(),
            "down" => "Move down".to_owned(),
            "back" => "Move to bottom".to_owned(),
            _ => e.title.to_owned(),
        };
    }
    if let ("scene.duplicate", Some("copied")) = (e.id, id.param("sources")) {
        return "Duplicate scene with new sources".to_owned();
    }
    if let ("ui.properties.show", Some(tab)) = (e.id, id.param("tab")) {
        return match tab {
            "filters" => "Filters…".to_owned(),
            "source" => "Choose another target…".to_owned(),
            "audio" => "Audio…".to_owned(),
            "transform" => "Transform…".to_owned(),
            _ => e.title.to_owned(),
        };
    }
    if let ("ui.settings.open", Some("settings.audio.loudness")) = (e.id, id.param("setting")) {
        return "Loudness settings…".to_owned();
    }
    // The vertical picture's panel: closing it only hides the picture, the vertical
    // output stays (its own item, Remove vertical output, follows in the panel menu).
    if let ("ui.panel.close", Some(VERTICAL_PANEL)) = (e.id, id.param("id")) {
        return "Hide vertical preview".to_owned();
    }
    if let ("scene.position", Some(n)) = (e.id, id.param("n")) {
        return format!("Scene at position {n}");
    }
    if let ("output.with_vertical", Some(kind)) = (e.id, id.param("kind")) {
        return match kind {
            "record" => "When recording".to_owned(),
            "stream" => "When going live".to_owned(),
            _ => e.title.to_owned(),
        };
    }
    match (e.id, id.param("at")) {
        ("ui.layout.studio_rail", Some("between")) => {
            return "Transition between the pictures".to_owned();
        }
        ("ui.layout.studio_rail", Some("below")) => {
            return "Transition below the pictures".to_owned();
        }
        _ => {}
    }
    match (e.id, id.param("area")) {
        ("ui.area.toggle", Some("left")) => "Left area".to_owned(),
        ("ui.area.toggle", Some("right")) => "Right area".to_owned(),
        ("ui.area.toggle", Some("bottom")) => "Bottom area".to_owned(),
        _ => match (e.id, id.param("id").and_then(|p| PanelId::parse(p).ok())) {
            ("ui.panel.maximize", Some(p)) => {
                let name = built_in(&p).map_or(p.as_str(), |i| i.title);
                format!("Maximize {name}")
            }
            // One panel of the Panels picker, as a context menu offers it.
            ("ui.panel.select", Some(p)) => built_in(&p).map_or(p.as_str(), |i| i.title).to_owned(),
            _ => e.title.to_owned(),
        },
    }
}
