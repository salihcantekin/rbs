//! Where a closed panel was, so that opening it again puts it back there
//! (ADR-0172 decision 2).
//!
//! The spot is kept in memory only, per workspace, while the app runs: it is not
//! written to the workspace file, so the file format does not change. After a
//! restart, a workspace switch or a reset, a closed panel opens in its default home
//! (ADR-0073 decision 4).

use super::{
    DEFAULT_FLYOUT, EdgeGroup, MAX_EDGE_GROUPS, MAX_GROUP_TABS, Placement, Side, Window,
    WindowContent, Workspace,
};
use crate::layout::{Axis, Edge, MAX_DEPTH, Node, Tabs};
use crate::panel::PanelId;

/// Most remembered spots; the oldest is forgotten first.
const MAX_SPOTS: usize = 64;

/// Smallest share a reopened stack takes of its neighbour's place, and leaves it.
const MIN_SHARE: f32 = 0.05;

/// The tree a closed panel was in.
#[derive(Clone, Debug, PartialEq)]
enum Home {
    /// The main window's centre.
    Centre,
    /// An area of the main window.
    Area(Side),
    /// An auto-hide edge group of the main window.
    Edge(Side),
    /// A floating window, with its last placement.
    Floating(Option<Placement>),
}

/// Where one panel was when it was closed.
#[derive(Clone, Debug, PartialEq)]
struct Spot {
    home: Home,
    /// The other panels of its tab stack (or edge group), in tab order.
    mates: Vec<PanelId>,
    /// Its tab index in that stack.
    index: usize,
    /// A panel of the neighbour its stack shared a split with, the side of that
    /// neighbour's stack the panel was on, and the panel's share of the two.
    anchor: Option<(PanelId, Edge, f32)>,
}

/// The spots of the panels closed while this workspace was open, oldest first.
///
/// It is not part of the layout: two workspaces that differ only in their spots are
/// equal, so comparing layouts (history, "modified" checks) ignores it.
#[derive(Clone, Debug, Default)]
pub struct ClosedSpots(Vec<(PanelId, Spot)>);

impl PartialEq for ClosedSpots {
    fn eq(&self, _: &ClosedSpots) -> bool {
        true
    }
}

impl ClosedSpots {
    fn take(&mut self, id: &PanelId) -> Option<Spot> {
        let i = self.0.iter().position(|(p, _)| p == id)?;
        Some(self.0.remove(i).1)
    }

    fn put(&mut self, id: PanelId, spot: Spot) {
        self.take(&id);
        if self.0.len() >= MAX_SPOTS {
            self.0.remove(0);
        }
        self.0.push((id, spot));
    }
}

impl Workspace {
    /// Remember where panel `id` is, before it is closed. The Pictures leaf and its
    /// pictures keep their own reopening rule (ADR-0073 decision 2) and are skipped.
    pub(super) fn remember_spot(&mut self, id: &PanelId) {
        if self.pictures_answer(id) || is_picture(id) {
            return;
        }
        if let Some(spot) = self.windows.iter().find_map(|w| spot_in(w, id)) {
            self.closed_spots.put(id.clone(), spot);
        }
    }

    /// Put closed panel `id` back where it was closed. Returns false (and changes
    /// nothing) when no spot is remembered or none of it can be found again; the
    /// caller then uses the default home.
    pub(super) fn reopen_at_spot(&mut self, id: &PanelId) -> bool {
        let Some(spot) = self.closed_spots.take(id) else {
            return false;
        };
        self.next_to_mates(id, &spot) || self.beside_anchor(id, &spot) || self.back_home(id, &spot)
    }

    /// Into the stack (or edge group) of a panel it shared a stack with, at its old
    /// tab index.
    fn next_to_mates(&mut self, id: &PanelId, spot: &Spot) -> bool {
        for mate in &spot.mates {
            if let Some((w, stack, _)) = self.find(mate) {
                let win = &mut self.windows[w];
                let Some((tree, local)) = win.locate(stack) else {
                    continue;
                };
                if let Some(t) = win.trees_mut().into_iter().nth(tree) {
                    return t.insert_into_stack(local, id.clone(), Some(spot.index));
                }
            }
            let Some(m) = self.main_layout_mut() else {
                continue;
            };
            if let Some(g) = m.group_of(mate) {
                let group = &mut m.auto_hide[g];
                if group.tabs.len() < MAX_GROUP_TABS {
                    let at = spot.index.min(group.tabs.len());
                    group.tabs.insert(at, id.clone());
                    group.active = at;
                    return true;
                }
            }
        }
        false
    }

    /// In a new stack on the same side of the neighbour it shared a split with.
    fn beside_anchor(&mut self, id: &PanelId, spot: &Spot) -> bool {
        let Some((anchor, edge, share)) = &spot.anchor else {
            return false;
        };
        for win in &mut self.windows {
            for tree in win.trees_mut() {
                let Some((local, _)) = tree.find(anchor) else {
                    continue;
                };
                let mut next = tree.clone();
                let stack = Tabs {
                    tabs: vec![id.clone()],
                    active: 0,
                    collapsed: false,
                };
                let share = share.clamp(MIN_SHARE, 1.0 - MIN_SHARE);
                if next.insert_beside_share(local, stack, *edge, share) && next.depth() <= MAX_DEPTH
                {
                    *tree = next;
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// Back into the area, edge group or floating window it was in. The centre has
    /// no such fallback: there the default home applies.
    fn back_home(&mut self, id: &PanelId, spot: &Spot) -> bool {
        match &spot.home {
            Home::Centre => false,
            Home::Area(side) => {
                let Some(m) = self.main_layout_mut() else {
                    return false;
                };
                let area = m.area_mut(*side);
                area.visible = true;
                match &mut area.layout {
                    Some(l) if l.stack_count() > 0 => l.insert_into_stack(0, id.clone(), None),
                    _ => {
                        area.layout = Some(Node::tabs(vec![id.clone()]));
                        true
                    }
                }
            }
            Home::Edge(side) => {
                let Some(m) = self.main_layout_mut() else {
                    return false;
                };
                if let Some(g) = m.auto_hide.iter_mut().find(|g| g.edge == *side) {
                    if g.tabs.len() < MAX_GROUP_TABS {
                        g.tabs.push(id.clone());
                        g.active = g.tabs.len() - 1;
                        return true;
                    }
                    return false;
                }
                if m.auto_hide.len() >= MAX_EDGE_GROUPS {
                    return false;
                }
                m.auto_hide.push(EdgeGroup {
                    edge: *side,
                    tabs: vec![id.clone()],
                    active: 0,
                    size: DEFAULT_FLYOUT,
                    anchor: None,
                });
                true
            }
            Home::Floating(placement) => {
                self.windows.push(Window {
                    content: WindowContent::Floating(Node::tabs(vec![id.clone()])),
                    placement: placement.clone(),
                });
                true
            }
        }
    }
}

/// Preview and Program of any canvas open by their own rules (ADR-0073, ADR-0093).
fn is_picture(id: &PanelId) -> bool {
    matches!(id.base(), "rbs.preview" | "rbs.program")
}

/// The spot of panel `id` in window `win`, if it is there.
fn spot_in(win: &Window, id: &PanelId) -> Option<Spot> {
    match &win.content {
        WindowContent::Main(m) => {
            let trees = std::iter::once((Home::Centre, &m.centre)).chain(
                Side::ALL
                    .into_iter()
                    .filter_map(|s| m.area(s).layout.as_ref().map(|l| (Home::Area(s), l))),
            );
            for (home, tree) in trees {
                if let Some(spot) = spot_in_tree(home, tree, id) {
                    return Some(spot);
                }
            }
            let g = &m.auto_hide[m.group_of(id)?];
            let index = g.tabs.iter().position(|p| p == id)?;
            Some(Spot {
                home: Home::Edge(g.edge),
                mates: g.tabs.iter().filter(|p| *p != id).cloned().collect(),
                index,
                anchor: None,
            })
        }
        WindowContent::Floating(n) => spot_in_tree(Home::Floating(win.placement.clone()), n, id),
        WindowContent::Projector(_) => None,
    }
}

fn spot_in_tree(home: Home, tree: &Node, id: &PanelId) -> Option<Spot> {
    let (stack, index) = tree.find(id)?;
    let tabs = tree.stack(stack)?;
    Some(Spot {
        home,
        mates: tabs.tabs.iter().filter(|p| *p != id).cloned().collect(),
        index,
        anchor: anchor_of(tree, id),
    })
}

/// What [`anchor_of`] found below a node.
enum Below {
    /// Panel `id` is not in this node.
    Absent,
    /// It is, and no neighbour with a tab stack was found yet.
    Alone,
    /// It is, with this anchor.
    Anchored(PanelId, Edge, f32),
}

/// A panel of the nearest neighbour of `id`'s stack that has a tab stack, the side
/// of that neighbour's stack `id` was on, and the share of the two that `id`'s part
/// of the split had. A neighbour before it is preferred.
fn anchor_of(tree: &Node, id: &PanelId) -> Option<(PanelId, Edge, f32)> {
    fn go(n: &Node, id: &PanelId) -> Below {
        match n {
            Node::Tabs(t) if t.tabs.contains(id) => Below::Alone,
            Node::Tabs(_) | Node::Pictures(_) => Below::Absent,
            Node::Split(s) => {
                let Some((i, below)) = s
                    .children
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (i, go(c, id)))
                    .find(|(_, b)| !matches!(b, Below::Absent))
                else {
                    return Below::Absent;
                };
                if let Below::Anchored(..) = below {
                    return below;
                }
                let (after_it, before_it) = match s.axis {
                    Axis::Horizontal => (Edge::Right, Edge::Left),
                    Axis::Vertical => (Edge::Bottom, Edge::Top),
                };
                let share = |n: usize| {
                    let (mine, theirs) = (s.ratios[i], s.ratios[n]);
                    mine / (mine + theirs)
                };
                if let Some(b) = i.checked_sub(1)
                    && let Some(p) = last_panel(&s.children[b])
                {
                    return Below::Anchored(p, after_it, share(b));
                }
                match s.children.get(i + 1).and_then(first_panel) {
                    Some(p) => Below::Anchored(p, before_it, share(i + 1)),
                    None => Below::Alone,
                }
            }
        }
    }
    match go(tree, id) {
        Below::Anchored(p, e, share) => Some((p, e, share)),
        _ => None,
    }
}

/// The first tab of the last tab stack of `n`.
fn last_panel(n: &Node) -> Option<PanelId> {
    let count = n.stack_count();
    n.stack(count.checked_sub(1)?)?.tabs.first().cloned()
}

/// The first tab of the first tab stack of `n`.
fn first_panel(n: &Node) -> Option<PanelId> {
    n.stack(0)?.tabs.first().cloned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::builtin;
    use crate::panel::id;

    /// The tree (0 centre, then areas with panels) and the stack of `p` in it.
    fn place(w: &Workspace, p: &'static str) -> (usize, Tabs) {
        let (win, stack, _) = w.find(&id(p)).expect("open");
        let (tree, _) = w.windows[win].locate(stack).unwrap();
        (tree, w.windows[win].stack(stack).unwrap().clone())
    }

    fn side_of(w: &Workspace, p: &'static str) -> Option<Side> {
        let m = w.main_layout().unwrap();
        Side::ALL.into_iter().find(|s| {
            m.area(*s)
                .layout
                .as_ref()
                .is_some_and(|l| l.find(&id(p)).is_some())
        })
    }

    fn stack_of(w: &Workspace, p: &'static str) -> usize {
        w.find(&id(p)).unwrap().1
    }

    #[test]
    fn a_tab_reopens_in_its_stack_at_its_index() {
        let mut w = builtin::edit();
        let props = stack_of(&w, "rbs.properties");
        w.move_to_stack(&id("rbs.mixer"), 0, props).unwrap();
        let before = place(&w, "rbs.mixer").1.tabs;
        assert_eq!(before, [id("rbs.properties"), id("rbs.mixer")]);
        w.close(&id("rbs.mixer")).unwrap();
        w.open(id("rbs.mixer")).unwrap();
        assert_eq!(side_of(&w, "rbs.mixer"), Some(Side::Right));
        let (_, t) = place(&w, "rbs.mixer");
        assert_eq!(t.tabs, before);
        assert_eq!(t.active, 1);
        w.validate().unwrap();
    }

    #[test]
    fn a_lone_stack_reopens_beside_its_neighbour() {
        let mut w = builtin::edit();
        // Sources sits under Scenes in the left area; the default home would make it
        // a tab of Scenes.
        w.close(&id("rbs.sources")).unwrap();
        w.open(id("rbs.sources")).unwrap();
        assert_eq!(place(&w, "rbs.sources").1.tabs, [id("rbs.sources")]);
        assert_eq!(side_of(&w, "rbs.sources"), Some(Side::Left));
        assert!(stack_of(&w, "rbs.sources") > stack_of(&w, "rbs.scenes"));
        assert_eq!(w, builtin::edit());
        w.validate().unwrap();
    }

    #[test]
    fn a_moved_panel_reopens_in_its_new_area_not_its_default_home() {
        let mut w = builtin::edit();
        let props = stack_of(&w, "rbs.properties");
        w.move_beside(&id("rbs.mixer"), 0, props, Edge::Bottom)
            .unwrap();
        w.close(&id("rbs.mixer")).unwrap();
        w.open(id("rbs.mixer")).unwrap();
        assert_eq!(side_of(&w, "rbs.mixer"), Some(Side::Right));
        assert_eq!(place(&w, "rbs.mixer").1.tabs, [id("rbs.mixer")]);
        assert!(stack_of(&w, "rbs.mixer") > stack_of(&w, "rbs.properties"));
        // With the neighbour gone too, the area it was in is still its home.
        w.close(&id("rbs.mixer")).unwrap();
        w.close(&id("rbs.properties")).unwrap();
        w.open(id("rbs.mixer")).unwrap();
        assert_eq!(side_of(&w, "rbs.mixer"), Some(Side::Right));
        w.validate().unwrap();
    }

    #[test]
    fn a_floating_panel_reopens_in_a_floating_window_where_it_was() {
        let mut w = builtin::edit();
        let at = Placement {
            rect: Some([10.0, 20.0, 300.0, 200.0]),
            ..Placement::default()
        };
        // Scenes is docked, so the spot is the floating window, not a docked mate.
        let f = w.float(&id("rbs.scenes")).unwrap();
        w.windows[f].placement = Some(at.clone());
        w.close(&id("rbs.scenes")).unwrap();
        assert_eq!(w.windows.len(), 1);
        w.open(id("rbs.scenes")).unwrap();
        let (win, ..) = w.find(&id("rbs.scenes")).unwrap();
        assert_ne!(win, 0);
        assert_eq!(w.windows[win].placement, Some(at));
        w.validate().unwrap();
    }

    #[test]
    fn an_auto_hidden_panel_reopens_in_its_edge_group() {
        let mut w = builtin::edit();
        w.close(&id("rbs.stats")).unwrap();
        w.open(id("rbs.stats")).unwrap();
        let m = w.main_layout().unwrap();
        let g = &m.auto_hide[m.group_of(&id("rbs.stats")).unwrap()];
        assert_eq!(g.tabs, [id("rbs.stats"), id("rbs.outputs")]);
        assert_eq!(g.active, 0);
        w.validate().unwrap();
    }

    #[test]
    fn spots_are_not_saved_and_do_not_change_equality() {
        let mut w = builtin::edit();
        let file = w.to_toml().unwrap();
        w.close(&id("rbs.mixer")).unwrap();
        let closed = w.clone();
        let again = Workspace::from_toml(&w.to_toml().unwrap()).unwrap();
        assert_eq!(again, closed);
        // Read from the file, the spot is gone: the default home applies.
        let mut fresh = again;
        fresh.open(id("rbs.mixer")).unwrap();
        assert_eq!(side_of(&fresh, "rbs.mixer"), Some(Side::Bottom));
        w.open(id("rbs.mixer")).unwrap();
        assert_eq!(w.to_toml().unwrap(), file);
    }
}
