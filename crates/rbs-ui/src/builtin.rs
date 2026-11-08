//! Built-in workspaces (UI rule U21): Edit, Live, Minimal and Audio.
//!
//! They are defaults: users may change them and restore them, never delete them.

use crate::layout::{Axis, Node, RailAt};
use crate::panel::id;
use crate::workspace::{DEFAULT_FLYOUT, EdgeGroup, Side, Workspace, same_name};

/// Names of the built-in workspaces, in menu order.
pub const NAMES: [&str; 4] = ["Edit", "Live", "Minimal", "Audio"];

fn tabs(ids: &[&'static str]) -> Node {
    Node::tabs(ids.iter().map(|i| id(i)).collect())
}

/// The studio split (ADR-0064): Preview alone outside studio mode; Preview, the
/// Transition rail and Program in studio mode.
fn studio() -> Node {
    Node::pictures(RailAt::Between)
}

/// The full editing layout of the design system's main window structure: the studio
/// split in the centre, Scenes and Sources in the left area, Properties in the
/// right area, the Mixer in the bottom area, and Stats and Outputs as edge tabs on
/// the right, and the Library on the left (ADR-0049, ADR-0064, ADR-0075, spec
/// Frame).
pub fn edit() -> Workspace {
    let mut ws = Workspace::new("Edit", studio());
    if let Some(m) = ws.main_layout_mut() {
        m.left.layout = Some(Node::split(
            Axis::Vertical,
            vec![
                (0.45, tabs(&["rbs.scenes"])),
                (0.55, tabs(&["rbs.sources"])),
            ],
        ));
        m.right.layout = Some(tabs(&["rbs.properties"]));
        m.bottom.layout = Some(tabs(&["rbs.mixer"]));
        m.auto_hide.push(EdgeGroup {
            edge: Side::Left,
            tabs: vec![id("rbs.library")],
            active: 0,
            size: DEFAULT_FLYOUT,
            anchor: Some(id("rbs.scenes")),
        });
        m.auto_hide.push(EdgeGroup {
            edge: Side::Right,
            tabs: vec![id("rbs.stats"), id("rbs.outputs")],
            active: 0,
            size: DEFAULT_FLYOUT,
            anchor: None,
        });
    }
    ws
}

/// The studio split over the mixer, live controls, room for chat.
pub fn live() -> Workspace {
    let views = studio();
    let left = Node::split(
        Axis::Vertical,
        vec![(0.68, views), (0.32, tabs(&["rbs.mixer"]))],
    );
    let right = tabs(&["rbs.stats", "rbs.scenes"]);
    Workspace::new(
        "Live",
        Node::split(Axis::Horizontal, vec![(0.76, left), (0.24, right)]),
    )
}

/// Program only; Go live, Record and the live indicator are in the window frame.
pub fn minimal() -> Workspace {
    Workspace::new("Minimal", tabs(&["rbs.program"]))
}

/// The mixer large, with a small program view.
pub fn audio() -> Workspace {
    Workspace::new(
        "Audio",
        Node::split(
            Axis::Vertical,
            vec![(0.3, tabs(&["rbs.program"])), (0.7, tabs(&["rbs.mixer"]))],
        ),
    )
}

/// Every built-in workspace, in [`NAMES`] order.
pub fn all() -> [Workspace; 4] {
    [edit(), live(), minimal(), audio()]
}

/// The default of the built-in workspace called `name` (case-insensitive).
pub fn by_name(name: &str) -> Option<Workspace> {
    all().into_iter().find(|w| same_name(&w.name, name))
}

/// Is `name` a built-in workspace name (case-insensitive)?
pub fn is_built_in(name: &str) -> bool {
    NAMES.iter().any(|n| same_name(n, name))
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
        assert_eq!(w.visible().len(), 1);
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
