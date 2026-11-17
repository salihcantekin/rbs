//! The icon set in Slint (spec `ui-design-system.md`, "Icons").
//!
//! The paths live in [`rbs_ui::icon::ICONS`]. The `IconName` enum and `Icons.named`
//! in `ui/base/types.slint` (shared types, ADR-0138) and the `Icon` component's paths
//! in `ui/components.slint` are generated from it by `types_text` and `icon_text`
//! (tests only) between marker lines; a test fails when a copy is out of date (run it
//! with `RBS_UPDATE_MENU_BAR=1` to rewrite it).
//!
//! Menus take their icons as images ([`menu_image`]): native menus (macOS, Windows)
//! cannot draw a Slint path.

use std::cell::RefCell;
use std::collections::HashMap;

/// The ink of menu icons: a neutral grey that reads on light and dark menus alike,
/// since the OS draws native menus in its own appearance, not the app theme's.
pub const MENU_INK: &str = "#8e8e93";

/// The pixel size menu icons are drawn at: Windows draws menu bitmaps at 16 x 16
/// whatever the scale; macOS shows them 18 points high (36 px on Retina); Slint's own
/// menus scale them down.
const MENU_PX: u32 = if cfg!(target_os = "windows") { 16 } else { 36 };

thread_local! {
    static MENU_IMAGES: RefCell<HashMap<&'static str, slint::Image>> = RefCell::new(HashMap::new());
}

/// The menu image of the icon `name`; an empty image for "" or a name not in the set.
/// Each is drawn once per thread (the UI thread) and kept.
pub fn menu_image(name: &str) -> slint::Image {
    let Some(icon) = rbs_ui::icon::find(name) else {
        return slint::Image::default();
    };
    MENU_IMAGES.with(|cache| {
        cache
            .borrow_mut()
            .entry(icon.name)
            .or_insert_with(|| {
                rbs_ui::icon::svg(icon.name, MENU_PX, MENU_INK)
                    .and_then(|svg| slint::Image::load_from_svg_data(svg.as_bytes()).ok())
                    .unwrap_or_default()
            })
            .clone()
    })
}

#[cfg(test)]
/// The marker lines around the generated `IconName` and `Icons` in `ui/base/types.slint`.
pub const TYPES_MARKERS: (&str, &str) = (
    "// BEGIN generated icon names (icons.rs; do not edit)",
    "// END generated icon names",
);
#[cfg(test)]
/// The marker lines around the generated `Icon` in `ui/components.slint`.
pub const ICON_MARKERS: (&str, &str) = (
    "// BEGIN generated icon (icons.rs; do not edit)",
    "// END generated icon",
);

#[cfg(test)]
/// The generated `IconName` and `Icons` (unindented).
pub fn types_text() -> String {
    use rbs_ui::icon::ICONS;
    let mut out = String::new();
    out.push_str(
        "// The icon set (docs/specs/ui-design-system.md, \"Icons\"; rbs_ui::icon::ICONS).\n",
    );
    out.push_str("export enum IconName {\n    none,\n");
    for i in ICONS {
        out.push_str(&format!("    {},\n", i.name));
    }
    out.push_str("}\n\n");
    out.push_str(
        "// An icon by its name in the icon set, as Rust passes it (registry and panel icons).\n\
         export global Icons {\n\
         \x20   // Slint has no string to enum conversion; unknown names draw nothing.\n\
         \x20   public pure function named(name: string) -> IconName {\n",
    );
    for (n, i) in ICONS.iter().enumerate() {
        let lead = if n == 0 {
            "        return "
        } else {
            "        : "
        };
        out.push_str(&format!(
            "{lead}name == \"{}\" ? IconName.{}\n",
            i.name, i.name
        ));
    }
    out.push_str("        : IconName.none;\n    }\n}\n");
    out
}

#[cfg(test)]
/// The generated `Icon` component (unindented).
pub fn icon_text() -> String {
    use rbs_ui::icon::ICONS;
    let mut out = String::new();
    out.push_str(
        "// Vector icon. Icons are paths, not font glyphs, so they look the same on every OS\n\
         // (a glyph missing from the system font shows as an empty box). Drawn in a 16 x 16\n\
         // view box. Decorative: the control that shows it carries the accessible name.\n\
         export component Icon inherits Rectangle {\n\
         \x20   in property <IconName> name;\n\
         \x20   in property <color> ink: Theme.text-primary;\n\
         \x20   // Filled shapes; the others are drawn as lines.\n",
    );
    let filled: Vec<String> = ICONS
        .iter()
        .filter(|i| i.filled)
        .map(|i| format!("name == IconName.{}", i.name))
        .collect();
    let filled = if filled.is_empty() {
        "false".to_owned()
    } else {
        filled.join(" || ")
    };
    out.push_str(&format!("    property <bool> filled: {filled};\n"));
    out.push_str("    property <string> commands:\n");
    for (n, i) in ICONS.iter().enumerate() {
        let lead = if n == 0 { "        " } else { "        : " };
        out.push_str(&format!(
            "{lead}name == IconName.{} ? \"{}\"\n",
            i.name, i.path
        ));
    }
    out.push_str("        : \"\";\n");
    out.push_str(
        "    width: Tokens.size-icon-box;\n\
         \x20   height: Tokens.size-icon-box;\n\
         \x20   accessible-role: none;\n\
         \x20   if commands != \"\": Path {\n\
         \x20       width: 100%;\n\
         \x20       height: 100%;\n\
         \x20       viewbox-width: 16;\n\
         \x20       viewbox-height: 16;\n\
         \x20       fill: root.filled ? root.ink : transparent;\n\
         \x20       stroke: root.filled ? transparent : root.ink;\n\
         \x20       stroke-width: root.filled ? 0px : Tokens.size-icon-stroke;\n\
         \x20       commands: root.commands;\n\
         \x20   }\n\
         }\n",
    );
    out
}

#[cfg(test)]
/// `text` with the part between `markers` replaced by `generated`, or `None` without
/// the marker pair.
pub fn refreshed(text: &str, markers: (&str, &str), generated: &str) -> Option<String> {
    let (begin_marker, end_marker) = markers;
    let begin = text.find(begin_marker)?;
    let after = begin + begin_marker.len();
    let end = after + text[after..].find(end_marker)?;
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..after]);
    out.push('\n');
    out.push_str(generated);
    out.push_str(&text[end..]);
    Some(out)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    /// Checks (or with `RBS_UPDATE_MENU_BAR`, rewrites) one generated copy.
    fn check_copy(file: &str, markers: (&str, &str), generated: &str) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
        let text = std::fs::read_to_string(&path).expect("the UI file is readable");
        let text = text.replace("\r\n", "\n");
        let fresh = refreshed(&text, markers, generated).expect("the UI file has the icon markers");
        if fresh != text && std::env::var_os("RBS_UPDATE_MENU_BAR").is_some() {
            std::fs::write(&path, &fresh).expect("the UI file is writable");
            return;
        }
        assert!(
            fresh == text,
            "the icon set in {file} is out of date: run \
             `RBS_UPDATE_MENU_BAR=1 cargo test -p rbs-app --bin rbs icons`"
        );
    }

    #[test]
    fn the_ui_files_have_the_current_icon_set() {
        check_copy("ui/base/types.slint", TYPES_MARKERS, &types_text());
        check_copy("ui/components.slint", ICON_MARKERS, &icon_text());
    }

    #[test]
    fn menu_images_are_drawn_once_and_unknown_names_are_empty() {
        let a = menu_image("plus");
        assert_eq!(a.size().width, MENU_PX);
        assert_eq!(a, menu_image("plus"));
        assert_eq!(menu_image("no-such-icon").size().width, 0);
        assert_eq!(menu_image("").size().width, 0);
    }

    #[test]
    fn every_icon_draws_as_a_menu_image() {
        for i in rbs_ui::icon::ICONS {
            assert_eq!(menu_image(i.name).size().width, MENU_PX, "{}", i.name);
        }
    }
}
