//! The icon set of the design system (`docs/specs/ui-design-system.md`, "Icons"):
//! vector paths in a 16 x 16 view box, toolkit-neutral. The Slint `Icon` component is
//! generated from [`ICONS`]; menus that take an image (native menus, Slint menus) get
//! one drawn from [`svg`].

/// One icon of the set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Icon {
    /// Its name, as registry entries and panels give it.
    pub name: &'static str,
    /// SVG path commands in a 16 x 16 view box.
    pub path: &'static str,
    /// Filled shape; the others are drawn as lines.
    pub filled: bool,
}

const fn line(name: &'static str, path: &'static str) -> Icon {
    Icon {
        name,
        path,
        filled: false,
    }
}

const fn fill(name: &'static str, path: &'static str) -> Icon {
    Icon {
        name,
        path,
        filled: true,
    }
}

/// Every icon, in the order of the spec's list.
pub const ICONS: &[Icon] = &[
    fill(
        "more",
        "M 2.5 8 a 1.25 1.25 0 1 0 2.5 0 a 1.25 1.25 0 1 0 -2.5 0 Z M 6.75 8 a 1.25 1.25 0 1 0 2.5 0 a 1.25 1.25 0 1 0 -2.5 0 Z M 11 8 a 1.25 1.25 0 1 0 2.5 0 a 1.25 1.25 0 1 0 -2.5 0 Z",
    ),
    line("chevron-down", "M 4 6 L 8 10 L 12 6"),
    line("chevron-up", "M 4 10 L 8 6 L 12 10"),
    line("chevron-left", "M 10 4 L 6 8 L 10 12"),
    line("chevron-right", "M 6 4 L 10 8 L 6 12"),
    line("check", "M 3.5 8.5 L 6.5 11.5 L 12.5 4.5"),
    line("close", "M 4.5 4.5 L 11.5 11.5 M 11.5 4.5 L 4.5 11.5"),
    fill("dot", "M 4 8 a 4 4 0 1 0 8 0 a 4 4 0 1 0 -8 0 Z"),
    line("ring", "M 4 8 a 4 4 0 1 0 8 0 a 4 4 0 1 0 -8 0 Z"),
    line("plus", "M 8 3 L 8 13 M 3 8 L 13 8"),
    line(
        "pin",
        "M 5.5 2.5 L 10.5 2.5 M 6.5 2.5 L 6.5 6.5 L 4.5 9.5 L 11.5 9.5 L 9.5 6.5 L 9.5 2.5 M 8 9.5 L 8 14",
    ),
    line(
        "panel-left",
        "M 2.5 3 L 13.5 3 L 13.5 13 L 2.5 13 Z M 6.5 3 L 6.5 13",
    ),
    line(
        "panel-right",
        "M 2.5 3 L 13.5 3 L 13.5 13 L 2.5 13 Z M 9.5 3 L 9.5 13",
    ),
    line(
        "panel-bottom",
        "M 2.5 3 L 13.5 3 L 13.5 13 L 2.5 13 Z M 2.5 9.5 L 13.5 9.5",
    ),
    line(
        "bolt",
        "M 9 1.5 L 3.5 9 L 7.5 9 L 7 14.5 L 12.5 7 L 8.5 7 Z",
    ),
    line(
        "grid",
        "M 2.5 2.5 L 7 2.5 L 7 7 L 2.5 7 Z M 9 2.5 L 13.5 2.5 L 13.5 7 L 9 7 Z M 2.5 9 L 7 9 L 7 13.5 L 2.5 13.5 Z M 9 9 L 13.5 9 L 13.5 13.5 L 9 13.5 Z",
    ),
    line(
        "copy",
        "M 5.5 2.5 L 13.5 2.5 L 13.5 10.5 L 5.5 10.5 Z M 2.5 5.5 L 2.5 13.5 L 10.5 13.5",
    ),
    line(
        "window",
        "M 2.5 3 L 13.5 3 L 13.5 13 L 2.5 13 Z M 2.5 6 L 13.5 6",
    ),
    line(
        "search",
        "M 3 7 a 4 4 0 1 0 8 0 a 4 4 0 1 0 -8 0 Z M 10 10 L 13.5 13.5",
    ),
    line(
        "monitor",
        "M 2 3 L 14 3 L 14 11 L 2 11 Z M 5.5 13.5 L 10.5 13.5 M 8 11 L 8 13.5",
    ),
    line(
        "camera",
        "M 2 4.5 L 10.5 4.5 L 10.5 11.5 L 2 11.5 Z M 10.5 7 L 14 5 L 14 11 L 10.5 9",
    ),
    line(
        "mic",
        "M 6 2.5 L 10 2.5 L 10 8.5 L 6 8.5 Z M 4 7.5 C 4 12 12 12 12 7.5 M 8 10.75 L 8 13.5 M 6 13.5 L 10 13.5",
    ),
    line(
        "speaker",
        "M 2.5 6 L 5 6 L 8.5 3 L 8.5 13 L 5 10 L 2.5 10 Z M 10.5 6 C 11.5 7 11.5 9 10.5 10 M 12 4.5 C 14 6.5 14 9.5 12 11.5",
    ),
    line(
        "eye",
        "M 1.5 8 C 4 3.5 12 3.5 14.5 8 C 12 12.5 4 12.5 1.5 8 Z M 6.25 8 a 1.75 1.75 0 1 0 3.5 0 a 1.75 1.75 0 1 0 -3.5 0 Z",
    ),
    line(
        "eye-off",
        "M 1.5 8 C 4 3.5 12 3.5 14.5 8 C 12 12.5 4 12.5 1.5 8 Z M 3 13 L 13 3",
    ),
    line(
        "expand",
        "M 2.5 6 L 2.5 2.5 L 6 2.5 M 10 2.5 L 13.5 2.5 L 13.5 6 M 13.5 10 L 13.5 13.5 L 10 13.5 M 6 13.5 L 2.5 13.5 L 2.5 10",
    ),
    line(
        "lock",
        "M 3.5 7 L 12.5 7 L 12.5 13.5 L 3.5 13.5 Z M 5.5 7 L 5.5 5 C 5.5 1.75 10.5 1.75 10.5 5 L 10.5 7",
    ),
    line(
        "unlock",
        "M 3.5 7 L 12.5 7 L 12.5 13.5 L 3.5 13.5 Z M 5.5 7 L 5.5 5 C 5.5 2 9.5 1.5 10.3 4",
    ),
    line(
        "speaker-off",
        "M 2.5 6 L 5 6 L 8.5 3 L 8.5 13 L 5 10 L 2.5 10 Z M 11 6 L 14 10 M 14 6 L 11 10",
    ),
    line(
        "sliders",
        "M 2.5 4.5 L 4 4.5 M 7 4.5 L 13.5 4.5 M 4 4.5 a 1.5 1.5 0 1 0 3 0 a 1.5 1.5 0 1 0 -3 0 Z M 2.5 11.5 L 9 11.5 M 12 11.5 L 13.5 11.5 M 9 11.5 a 1.5 1.5 0 1 0 3 0 a 1.5 1.5 0 1 0 -3 0 Z",
    ),
    line(
        "palette",
        "M 8 2 C 4.5 2 2 4.5 2 7.75 C 2 11 4.5 14 7.5 14 C 8.5 14 9 13.25 8.75 12.5 C 8.25 11.25 9 10 10.25 10 L 11.5 10 C 13 10 14 9 14 7.5 C 14 4.5 11.25 2 8 2 Z M 4.75 7.5 a 0.75 0.75 0 1 0 1.5 0 a 0.75 0.75 0 1 0 -1.5 0 Z M 7 4.75 a 0.75 0.75 0 1 0 1.5 0 a 0.75 0.75 0 1 0 -1.5 0 Z M 9.75 6 a 0.75 0.75 0 1 0 1.5 0 a 0.75 0.75 0 1 0 -1.5 0 Z",
    ),
    line(
        "keyboard",
        "M 1.5 4 L 14.5 4 L 14.5 12 L 1.5 12 Z M 4 6.75 L 4.5 6.75 M 6.5 6.75 L 7 6.75 M 9 6.75 L 9.5 6.75 M 11.5 6.75 L 12 6.75 M 5 9.5 L 11 9.5",
    ),
    line(
        "broadcast",
        "M 6.75 8 a 1.25 1.25 0 1 0 2.5 0 a 1.25 1.25 0 1 0 -2.5 0 Z M 5.25 5.25 C 3.75 6.75 3.75 9.25 5.25 10.75 M 10.75 5.25 C 12.25 6.75 12.25 9.25 10.75 10.75 M 3.25 3.25 C 0.75 5.75 0.75 10.25 3.25 12.75 M 12.75 3.25 C 15.25 5.75 15.25 10.25 12.75 12.75",
    ),
    line(
        "link",
        "M 6.5 9.5 L 9.5 6.5 M 7 4.5 L 8.5 3 C 9.75 1.75 11.75 1.75 13 3 C 14.25 4.25 14.25 6.25 13 7.5 L 11.5 9 M 9 11.5 L 7.5 13 C 6.25 14.25 4.25 14.25 3 13 C 1.75 11.75 1.75 9.75 3 8.5 L 4.5 7",
    ),
    line(
        "globe",
        "M 2 8 a 6 6 0 1 0 12 0 a 6 6 0 1 0 -12 0 Z M 2 8 L 14 8 M 8 2 C 5.75 4 5.75 12 8 14 M 8 2 C 10.25 4 10.25 12 8 14",
    ),
    line(
        "gear",
        "M 12.68 6.24 L 14.47 6.69 L 14.47 9.31 L 12.68 9.76 L 12.55 10.06 L 13.50 11.65 L 11.65 13.50 L 10.06 12.55 L 9.76 12.68 L 9.31 14.47 L 6.69 14.47 L 6.24 12.68 L 5.94 12.55 L 4.35 13.50 L 2.50 11.65 L 3.45 10.06 L 3.32 9.76 L 1.53 9.31 L 1.53 6.69 L 3.32 6.24 L 3.45 5.94 L 2.50 4.35 L 4.35 2.50 L 5.94 3.45 L 6.24 3.32 L 6.69 1.53 L 9.31 1.53 L 9.76 3.32 L 10.06 3.45 L 11.65 2.50 L 13.50 4.35 L 12.55 5.94 Z M 6 8 a 2 2 0 1 0 4 0 a 2 2 0 1 0 -4 0 Z",
    ),
    line(
        "user",
        "M 5.5 5 a 2.5 2.5 0 1 0 5 0 a 2.5 2.5 0 1 0 -5 0 Z M 2.5 14 C 2.5 9.5 13.5 9.5 13.5 14",
    ),
    line(
        "folder",
        "M 1.5 3.5 L 6 3.5 L 7.5 5 L 14.5 5 L 14.5 12.5 L 1.5 12.5 Z",
    ),
    line(
        "undo",
        "M 5.5 3.5 L 2.5 6.5 L 5.5 9.5 M 2.5 6.5 L 10 6.5 C 14 6.5 14 12.5 10 12.5 L 7 12.5",
    ),
    line(
        "redo",
        "M 10.5 3.5 L 13.5 6.5 L 10.5 9.5 M 13.5 6.5 L 6 6.5 C 2 6.5 2 12.5 6 12.5 L 9 12.5",
    ),
    line(
        "layout",
        "M 2.5 3 L 13.5 3 L 13.5 13 L 2.5 13 Z M 2.5 6 L 13.5 6 M 6.5 6 L 6.5 13",
    ),
    line(
        "panel-all",
        "M 2.5 3 L 13.5 3 L 13.5 13 L 2.5 13 Z M 5.5 3 L 5.5 10 M 10.5 3 L 10.5 10 M 2.5 10 L 13.5 10",
    ),
    line(
        "panel-none",
        "M 2.5 3 L 13.5 3 L 13.5 13 L 2.5 13 Z M 5.5 6 L 10.5 6 L 10.5 10 L 5.5 10 Z",
    ),
    line(
        "reset",
        "M 3.5 8 a 4.5 4.5 0 1 0 1.32 -3.18 M 4.82 2.3 L 4.82 4.82 L 7.3 4.82",
    ),
    line(
        "refresh",
        "M 12.5 8 a 4.5 4.5 0 1 1 -1.32 -3.18 M 11.18 2.3 L 11.18 4.82 L 8.7 4.82",
    ),
    line(
        "image",
        "M 2 3 L 14 3 L 14 13 L 2 13 Z M 2 11 L 6 7.5 L 9 10 L 11 8.5 L 14 11 M 10 5.75 a 1 1 0 1 0 2 0 a 1 1 0 1 0 -2 0 Z",
    ),
    line(
        "pencil",
        "M 10.5 2.5 L 13.5 5.5 L 5.5 13.5 L 2.5 13.5 L 2.5 10.5 Z M 9 4 L 12 7",
    ),
    line(
        "trash",
        "M 2.5 4 L 13.5 4 M 6 4 L 6 2.5 L 10 2.5 L 10 4 M 4 4 L 4.75 13.5 L 11.25 13.5 L 12 4 M 6.75 6.5 L 6.75 11 M 9.25 6.5 L 9.25 11",
    ),
    line(
        "transition",
        "M 1.5 4.5 L 7 4.5 L 7 11.5 L 1.5 11.5 Z M 9 4.5 L 14.5 4.5 L 14.5 11.5 L 9 11.5 Z M 7 8 L 9 8",
    ),
    line("arrow-left", "M 13 8 L 3 8 M 7 4 L 3 8 L 7 12"),
    line("arrow-right", "M 3 8 L 13 8 M 9 4 L 13 8 L 9 12"),
    line(
        "menu",
        "M 2.5 4 L 13.5 4 M 2.5 8 L 13.5 8 M 2.5 12 L 13.5 12",
    ),
    line("window-minimize", "M 3.5 8 L 12.5 8"),
    line(
        "window-maximize",
        "M 3.5 3.5 L 12.5 3.5 L 12.5 12.5 L 3.5 12.5 Z",
    ),
    line(
        "window-restore",
        "M 3.5 5.5 L 10.5 5.5 L 10.5 12.5 L 3.5 12.5 Z M 5.5 5.5 L 5.5 3.5 L 12.5 3.5 L 12.5 10.5 L 10.5 10.5",
    ),
];

/// The icon named `name`, if the set has it.
pub fn find(name: &str) -> Option<&'static Icon> {
    ICONS.iter().find(|i| i.name == name)
}

/// The stroke width of line icons in view box units. A little heavier than
/// `size.icon.stroke`, since menus draw their icons smaller than `size.icon.box`.
pub const STROKE: f32 = 1.25;

/// `name` as a standalone SVG document `px` pixels square, drawn in `ink` (`#rrggbb`),
/// for toolkits that take menu icons as images. `None` for a name not in the set.
pub fn svg(name: &str, px: u32, ink: &str) -> Option<String> {
    let icon = find(name)?;
    let paint = if icon.filled {
        format!("fill=\"{ink}\" stroke=\"none\"")
    } else {
        format!(
            "fill=\"none\" stroke=\"{ink}\" stroke-width=\"{STROKE}\" stroke-linecap=\"round\" stroke-linejoin=\"round\""
        )
    };
    Some(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{px}\" height=\"{px}\" viewBox=\"0 0 16 16\"><path d=\"{}\" {paint}/></svg>",
        icon.path
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_kebab_case() {
        for (i, a) in ICONS.iter().enumerate() {
            assert!(
                a.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{}",
                a.name
            );
            assert!(!a.path.is_empty(), "{}", a.name);
            assert!(
                ICONS[i + 1..].iter().all(|b| b.name != a.name),
                "{}",
                a.name
            );
        }
    }

    #[test]
    fn paths_stay_in_the_view_box() {
        // Absolute coordinates only (lower-case commands are relative arcs from a
        // point already checked).
        for i in ICONS {
            let mut absolute = true;
            for word in i.path.split_whitespace() {
                match word {
                    "M" | "L" | "C" | "Z" => absolute = true,
                    "a" => absolute = false,
                    w if absolute => {
                        let v: f32 = w.parse().unwrap_or(f32::NAN);
                        assert!((0.0..=16.0).contains(&v), "{}: {w}", i.name);
                    }
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn svg_draws_lines_and_fills() {
        let s = svg("plus", 32, "#8e8e93").unwrap_or_default();
        assert!(
            s.contains("width=\"32\"") && s.contains("stroke=\"#8e8e93\""),
            "{s}"
        );
        let s = svg("dot", 16, "#000000").unwrap_or_default();
        assert!(s.contains("fill=\"#000000\" stroke=\"none\""), "{s}");
        assert_eq!(svg("no-such-icon", 16, "#000000"), None);
    }
}
