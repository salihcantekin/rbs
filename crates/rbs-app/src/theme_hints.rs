//! Words for the theme editor (themes spec, "Settings › Appearance").

/// What a length token shapes, in words, for its row in the theme editor: the user
/// sees which parts of the window move when it changes. Empty when the label says it.
pub fn shapes(name: &str) -> &'static str {
    match name {
        "radius.sm" => "Fields, drop-downs, checkboxes, list rows",
        "radius.md" => "Switches, segmented controls, tabs, menus, selected rows",
        "radius.lg" => "Settings cards and dialog cards",
        "radius.button" => "Buttons with text; at most half the small control height",
        "size.button.chip" => "The plate behind a button's icon",
        "size.border" => "Outlines of fields and cards, and dividers",
        "space.1" => "The smallest gaps inside controls",
        "space.2" => "Gaps inside controls and rows",
        "space.3" => "Gaps between the parts of a control",
        "space.4" => "Gaps around groups of rows",
        "space.6" | "space.8" => "Large gaps in pages and popups",
        "size.panel.pad" => "Inside docked panels and Settings cards",
        "size.section.gap" => "Between sections of the studio panels",
        "size.field.gap" => "Between rows of fields",
        "size.control.sm" => "Dense buttons, icon buttons, small fields",
        "size.control.md" => "Buttons, fields, drop-downs, list rows",
        "size.control.lg" => "Large rows and buttons",
        _ => "",
    }
}
