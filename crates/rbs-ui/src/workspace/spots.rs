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
use std::sync::Arc;

use crate::layout::{Axis, Edge, MAX_DEPTH, Node, Split, Tabs};
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
    /// The neighbour its stack (or the part of the tree holding it) shared a split
    /// with.
    anchor: Option<Neighbour>,
}

/// The neighbour of a closed panel's part of the tree.
#[derive(Clone, Debug, PartialEq)]
struct Neighbour {
    /// Every panel of the neighbour node, sorted: the node is found again by them.
    panels: Vec<PanelId>,
    /// The side of the neighbour the panel was on.
    edge: Edge,
    /// The panel's part's share of the two.
    share: f32,
}

/// The spots of the panels closed while this workspace was open, oldest first.
///
/// It is not part of the layout: two workspaces that differ only in their spots are
/// equal, so comparing layouts (history, "modified" checks) ignores it. On purpose,
/// a command that changed only spots is then no layout change and is dropped by the
/// session; spots change only with a close or an open, which change the layout too.
/// Shared between clones (each layout history step holds one) until one changes.
#[derive(Clone, Debug, Default)]
pub struct ClosedSpots(Arc<Vec<(PanelId, Spot)>>);

impl PartialEq for ClosedSpots {
    fn eq(&self, _: &ClosedSpots) -> bool {
        true
    }
}

impl ClosedSpots {
    fn take(&mut self, id: &PanelId) -> Option<Spot> {
        let i = self.0.iter().position(|(p, _)| p == id)?;
        Some(Arc::make_mut(&mut self.0).remove(i).1)
    }

    fn put(&mut self, id: PanelId, spot: Spot) {
        self.take(&id);
        let all = Arc::make_mut(&mut self.0);
        if all.len() >= MAX_SPOTS {
            all.remove(0);
        }
        all.push((id, spot));
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
                if let Some(t) = win.trees_mut().into_iter().nth(tree)
                    && t.insert_into_stack(local, id.clone(), Some(spot.index))
                {
                    return true;
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

    /// In a new stack on the same side of the neighbour it shared a split with, at its
    /// old share: beside that whole node when it is found again by its panels, else
    /// beside the stack of one of them.
    fn beside_anchor(&mut self, id: &PanelId, spot: &Spot) -> bool {
        let Some(n) = &spot.anchor else {
            return false;
        };
        let share = n.share.clamp(MIN_SHARE, 1.0 - MIN_SHARE);
        let stack = Tabs {
            tabs: vec![id.clone()],
            active: 0,
            collapsed: false,
        };
        for win in &mut self.windows {
            for tree in win.trees_mut() {
                let mut next = tree.clone();
                let placed = insert_beside_node(&mut next, &n.panels, &stack, n.edge, share)
                    || n.panels
                        .iter()
                        .find_map(|p| next.find(p))
                        .is_some_and(|(local, _)| {
                            next.insert_beside_share(local, stack.clone(), n.edge, share)
                        });
                if placed {
                    if next.depth() > MAX_DEPTH {
                        return false;
                    }
                    *tree = next;
                    return true;
                }
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
                        let at = spot.index.min(g.tabs.len());
                        g.tabs.insert(at, id.clone());
                        g.active = at;
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
    /// It is, and no neighbour was found yet.
    Alone,
    /// It is, with this neighbour.
    Anchored(Neighbour),
}

/// The node next to the part of the tree that holds `id`'s stack, at the lowest
/// split that has one (the one before it preferred), with the side and share.
fn anchor_of(tree: &Node, id: &PanelId) -> Option<Neighbour> {
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
                let (b, edge) = match i.checked_sub(1) {
                    Some(b) => (b, after_it),
                    None if i + 1 < s.children.len() => (i + 1, before_it),
                    None => return Below::Alone,
                };
                let (mine, theirs) = (s.ratios[i], s.ratios[b]);
                Below::Anchored(Neighbour {
                    panels: sorted_panels(&s.children[b]),
                    edge,
                    share: mine / (mine + theirs),
                })
            }
        }
    }
    match go(tree, id) {
        Below::Anchored(n) => Some(n),
        _ => None,
    }
}

fn sorted_panels(n: &Node) -> Vec<PanelId> {
    let mut all: Vec<PanelId> = n.panels().into_iter().cloned().collect();
    all.sort();
    all
}

/// Put `stack` on the `edge` side of the node whose panels are exactly `panels`,
/// taking `share` of its place: inside its parent split when that split runs along
/// the edge's axis, else in a new split in its place. Returns false (and leaves the
/// tree alone) when there is no such node.
fn insert_beside_node(
    tree: &mut Node,
    panels: &[PanelId],
    stack: &Tabs,
    edge: Edge,
    share: f32,
) -> bool {
    let (axis, first) = match edge {
        Edge::Left => (Axis::Horizontal, true),
        Edge::Right => (Axis::Horizontal, false),
        Edge::Top => (Axis::Vertical, true),
        Edge::Bottom => (Axis::Vertical, false),
    };
    let new = || Node::Tabs(stack.clone());
    if sorted_panels(tree) == panels {
        let old = std::mem::replace(tree, Node::empty());
        let parts = if first {
            vec![(share, new()), (1.0 - share, old)]
        } else {
            vec![(1.0 - share, old), (share, new())]
        };
        *tree = Node::split(axis, parts);
        return true;
    }
    let Node::Split(Split {
        axis: a,
        ratios,
        children,
    }) = tree
    else {
        return false;
    };
    if *a == axis
        && let Some(i) = children.iter().position(|c| sorted_panels(c) == panels)
    {
        let part = ratios[i] * share;
        ratios[i] -= part;
        let at = if first { i } else { i + 1 };
        ratios.insert(at, part);
        children.insert(at, new());
        return true;
    }
    children
        .iter_mut()
        .any(|c| insert_beside_node(c, panels, stack, edge, share))
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

    #[test]
    fn a_stack_beside_a_nested_split_reopens_beside_the_whole_split() {
        let mut w = builtin::edit();
        // Left area: Scenes over Sources, with Properties right of that whole split.
        let m = w.main_layout_mut().unwrap();
        let old = m.left.layout.take().unwrap();
        m.left.layout = Some(Node::split(
            Axis::Horizontal,
            vec![(0.7, old), (0.3, Node::tabs(vec![id("rbs.properties")]))],
        ));
        m.right.layout = None;
        let before = w.clone();
        w.close(&id("rbs.properties")).unwrap();
        w.open(id("rbs.properties")).unwrap();
        let (l, r) = (
            w.main_layout().unwrap().left.layout.clone(),
            before.main_layout().unwrap().left.layout.clone(),
        );
        let ratios = |n: &Option<Node>| match n {
            Some(Node::Split(s)) => (s.axis, s.ratios.clone(), s.children.len()),
            _ => panic!("a split"),
        };
        let (axis, got, n) = ratios(&l);
        assert_eq!((axis, n), (Axis::Horizontal, 2));
        assert!((got[0] - 0.7).abs() < 1e-4);
        assert_eq!(
            sorted_panels(l.as_ref().unwrap()),
            sorted_panels(r.as_ref().unwrap())
        );
        w.validate().unwrap();
    }

    #[test]
    fn a_closed_floating_window_comes_back_as_one_window() {
        let mut w = builtin::edit();
        let f = w.float(&id("rbs.scenes")).unwrap();
        let stack = w.find(&id("rbs.scenes")).unwrap().1;
        w.move_to_stack(&id("rbs.sources"), f, stack).unwrap();
        w.close_window(f).unwrap();
        w.open(id("rbs.scenes")).unwrap();
        w.open(id("rbs.sources")).unwrap();
        assert_eq!(w.windows.len(), 2);
        assert_eq!(
            place(&w, "rbs.sources").1.tabs,
            [id("rbs.scenes"), id("rbs.sources")]
        );
        w.validate().unwrap();
    }

    #[test]
    fn undo_and_redo_of_a_close_and_a_reopen() {
        use crate::session::{LayoutCommand, LayoutSession, Origin};
        let mut w = builtin::edit();
        let props = stack_of(&w, "rbs.properties");
        w.move_to_stack(&id("rbs.mixer"), 0, props).unwrap();
        let start = w.clone();
        let mut s = LayoutSession::new(w);
        let mixer = id("rbs.mixer");
        s.apply(LayoutCommand::ClosePanel(mixer.clone()), Origin::Command)
            .unwrap();
        assert!(s.undo());
        assert_eq!(s.workspace(), &start);
        assert!(s.redo());
        assert!(!s.workspace().is_open(&mixer));
        s.apply(LayoutCommand::OpenPanel(mixer.clone()), Origin::Command)
            .unwrap();
        assert_eq!(s.workspace(), &start);
        assert!(s.undo());
        // Undoing the reopen gives the spot back, so it reopens there again.
        s.apply(LayoutCommand::OpenPanel(mixer), Origin::Command)
            .unwrap();
        assert_eq!(s.workspace(), &start);
    }
}
