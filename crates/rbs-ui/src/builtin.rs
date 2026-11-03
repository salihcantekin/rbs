//! Built-in workspaces (UI rule U21): Edit, Live, Minimal and Audio.
//!
//! They are defaults: users may change them and restore them, never delete them.

use crate::layout::{Axis, Node};
use crate::panel::id;
use crate::workspace::Workspace;

/// Names of the built-in workspaces, in menu order.
pub const NAMES: [&str; 4] = ["Edit", "Live", "Minimal", "Audio"];

fn tabs(ids: &[&'static str]) -> Node {
    Node::tabs(ids.iter().map(|i| id(i)).collect())
}

/// The full editing layout of the design system's main window structure.
pub fn edit() -> Workspace {
    let top = Node::split(
        Axis::Horizontal,
        vec![
            (
                0.2,
                Node::split(
                    Axis::Vertical,
                    vec![(0.5, tabs(&["rbs.scenes"])), (0.5, tabs(&["rbs.sources"]))],
                ),
            ),
            (0.58, tabs(&["rbs.preview"])),
            (0.22, tabs(&["rbs.properties"])),
        ],
    );
    let bottom = Node::split(
        Axis::Horizontal,
        vec![
            (0.6, tabs(&["rbs.mixer", "rbs.stats", "rbs.outputs"])),
            (0.2, tabs(&["rbs.transition"])),
            (0.2, tabs(&["rbs.live-controls"])),
        ],
    );
    Workspace::new(
        "Edit",
        Node::split(Axis::Vertical, vec![(0.7, top), (0.3, bottom)]),
    )
}

/// Preview and program side by side, live controls, room for chat, and the mixer.
pub fn live() -> Workspace {
    let views = Node::split(
        Axis::Horizontal,
        vec![(0.5, tabs(&["rbs.preview"])), (0.5, tabs(&["rbs.program"]))],
    );
    let left = Node::split(
        Axis::Vertical,
        vec![(0.68, views), (0.32, tabs(&["rbs.mixer"]))],
    );
    let right = Node::split(
        Axis::Vertical,
        vec![
            (0.4, tabs(&["rbs.live-controls", "rbs.transition"])),
            (0.6, tabs(&["rbs.stats", "rbs.scenes"])),
        ],
    );
    Workspace::new(
        "Live",
        Node::split(Axis::Horizontal, vec![(0.76, left), (0.24, right)]),
    )
}

/// Program and live controls only; the live indicator is in the window frame.
pub fn minimal() -> Workspace {
    Workspace::new(
        "Minimal",
        Node::split(
            Axis::Vertical,
            vec![
                (0.8, tabs(&["rbs.program"])),
                (0.2, tabs(&["rbs.live-controls"])),
            ],
        ),
    )
}

/// The mixer large, with a small program view and live controls.
pub fn audio() -> Workspace {
    let top = Node::split(
        Axis::Horizontal,
        vec![
            (0.6, tabs(&["rbs.program"])),
            (0.4, tabs(&["rbs.live-controls"])),
        ],
    );
    Workspace::new(
        "Audio",
        Node::split(
            Axis::Vertical,
            vec![(0.3, top), (0.7, tabs(&["rbs.mixer"]))],
        ),
    )
}

/// Every built-in workspace, in [`NAMES`] order.
pub fn all() -> [Workspace; 4] {
    [edit(), live(), minimal(), audio()]
}

/// The default of the built-in workspace called `name` (case-insensitive).
pub fn by_name(name: &str) -> Option<Workspace> {
    all()
        .into_iter()
        .find(|w| w.name.eq_ignore_ascii_case(name))
}

/// Is `name` a built-in workspace name (case-insensitive)?
pub fn is_built_in(name: &str) -> bool {
    NAMES.iter().any(|n| n.eq_ignore_ascii_case(name))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::panel::{UpdateRate, needed_rate};

    #[test]
    fn built_ins_are_valid_and_round_trip() {
        for (w, name) in all().iter().zip(NAMES) {
            assert_eq!(w.name, name);
            w.validate().unwrap();
            let text = w.to_toml().unwrap();
            let back = Workspace::from_toml(&text).unwrap();
            assert_eq!(&back, w, "{text}");
        }
    }

    #[test]
    fn minimal_is_cheap() {
        // Minimal needs frames only for the program view; nothing else animates.
        let w = minimal();
        assert_eq!(w.visible().len(), 2);
        assert_eq!(needed_rate(w.visible()), UpdateRate::Display);
        let mut w = minimal();
        w.close(&id("rbs.program")).unwrap();
        assert_eq!(needed_rate(w.visible()), UpdateRate::OnChange);
    }

    #[test]
    fn names_are_case_insensitive() {
        assert!(is_built_in("minimal"));
        assert_eq!(by_name("LIVE").map(|w| w.name), Some("Live".to_owned()));
        assert!(by_name("Mine").is_none());
    }
}
