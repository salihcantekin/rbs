//! The layout tree of one window: splits whose leaves are tab stacks.
//!
//! Spec: `docs/specs/ui-design-system.md`, "Panel model" and "Workspace file".
//! Tab stacks are addressed by their depth-first index in the tree, which is
//! what [`Node::flatten`] reports for each stack.

use serde::{Deserialize, Serialize};

use crate::panel::PanelId;

/// Deepest nesting of splits accepted in a file.
pub const MAX_DEPTH: usize = 16;

/// Thickness of a splitter handle in logical pixels.
pub const HANDLE_THICKNESS: f32 = 6.0;

/// Length of a collapsed tab stack along its split: its title row (`size.control.lg`)
/// plus its border (ADR-0049 decision 2).
pub const COLLAPSED_STACK: f32 = 33.0;

/// Smallest length a pane can be dragged to, in logical pixels.
pub const MIN_PANE: f32 = 48.0;

/// Share of the window a panel gets when it is moved to an edge.
pub const EDGE_SHARE: f32 = 0.25;

/// Width of the Transition rail between the two pictures (ADR-0064).
pub const RAIL_BETWEEN: f32 = 132.0;

/// Height of the Transition rail below the two pictures (ADR-0064).
pub const RAIL_BELOW: f32 = 56.0;

/// Where a studio split puts the Transition rail in studio mode (ADR-0064).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RailAt {
    /// Between Preview and Program.
    Between,
    /// Below both pictures, centred.
    Below,
}

/// Direction of a split. `Horizontal` places children side by side,
/// `Vertical` stacks them top to bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// Children left to right.
    Horizontal,
    /// Children top to bottom.
    Vertical,
}

/// An edge of a window, for "move to an edge" (UI rule U21).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// Left edge.
    Left,
    /// Right edge.
    Right,
    /// Top edge.
    Top,
    /// Bottom edge.
    Bottom,
}

/// The split axis along an edge, and whether the edge's side comes first in it.
fn edge_axis(edge: Edge) -> (Axis, bool) {
    match edge {
        Edge::Left => (Axis::Horizontal, true),
        Edge::Right => (Axis::Horizontal, false),
        Edge::Top => (Axis::Vertical, true),
        Edge::Bottom => (Axis::Vertical, false),
    }
}

/// A split: two or more children with one ratio each. Ratios are positive and sum to 1.
#[derive(Clone, Debug, PartialEq)]
pub struct Split {
    /// Direction.
    pub axis: Axis,
    /// Share of each child.
    pub ratios: Vec<f32>,
    /// Children.
    pub children: Vec<Node>,
    /// A studio split (ADR-0064): Preview then Program, with the Transition rail
    /// here in studio mode. Kept only while [`Split::is_studio_shape`] holds.
    pub studio: Option<RailAt>,
}

impl Split {
    /// Two single-tab stacks, not collapsed, side by side: Preview and then the
    /// Program of the same canvas.
    pub fn is_studio_shape(&self) -> bool {
        let one = |n: &Node| match n {
            Node::Tabs(t) if t.tabs.len() == 1 && !t.collapsed => t.tabs.first().cloned(),
            _ => None,
        };
        let [a, b] = self.children.as_slice() else {
            return false;
        };
        let (Some(p), Some(q)) = (one(a), one(b)) else {
            return false;
        };
        self.axis == Axis::Horizontal
            && p.base() == "rbs.preview"
            && q.base() == "rbs.program"
            && p.param("canvas") == q.param("canvas")
    }
}

/// A tab stack. Only the root of a window may be empty (every panel closed).
#[derive(Clone, Debug, PartialEq)]
pub struct Tabs {
    /// Panels, in tab order.
    pub tabs: Vec<PanelId>,
    /// Index of the visible tab.
    pub active: usize,
    /// Only the title row is shown (ADR-0049 decision 2, file `state = "collapsed"`).
    pub collapsed: bool,
}

/// The state a tab stack has in a file; `docked` is the default and is not written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StackState {
    Docked,
    Collapsed,
}

/// A layout tree node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawNode", into = "RawNode")]
pub enum Node {
    /// A split.
    Split(Split),
    /// A tab stack.
    Tabs(Tabs),
}

/// File shape of a node: `{ split, ratios, children }` or `{ tabs, active }`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    split: Option<Axis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ratios: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    children: Option<Vec<Node>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tabs: Option<Vec<PanelId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<StackState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    studio: Option<RailAt>,
}

impl TryFrom<RawNode> for Node {
    type Error = String;

    fn try_from(r: RawNode) -> Result<Self, String> {
        match (r.split, r.ratios, r.children, r.tabs, r.active) {
            (Some(_), _, _, None, None) if r.state.is_some() => {
                Err("only a tab stack has a `state`".into())
            }
            (None, _, _, _, _) if r.studio.is_some() => Err("only a split has `studio`".into()),
            (Some(axis), Some(ratios), Some(children), None, None) => {
                if children.len() < 2 {
                    return Err("a split needs at least two children".into());
                }
                if ratios.len() != children.len() {
                    return Err(format!(
                        "a split has {} children but {} ratios",
                        children.len(),
                        ratios.len()
                    ));
                }
                if ratios.iter().any(|r| !r.is_finite() || *r <= 0.0) {
                    return Err("split ratios must be positive numbers".into());
                }
                let sum: f32 = ratios.iter().sum();
                if (sum - 1.0).abs() > 0.01 {
                    return Err(format!("split ratios must sum to 1, not {sum}"));
                }
                let ratios = ratios.iter().map(|r| r / sum).collect();
                let split = Split {
                    axis,
                    ratios,
                    children,
                    studio: r.studio,
                };
                if split.studio.is_some() && !split.is_studio_shape() {
                    return Err("a studio split is horizontal with two single-tab stacks: \
                         Preview, then the Program of the same canvas"
                        .into());
                }
                Ok(Node::Split(split))
            }
            (None, None, None, Some(tabs), active) => {
                let active = active.unwrap_or(0);
                if active >= tabs.len().max(1) {
                    return Err(format!(
                        "active tab {active} is out of range for {} tabs",
                        tabs.len()
                    ));
                }
                Ok(Node::Tabs(Tabs {
                    tabs,
                    active,
                    collapsed: r.state == Some(StackState::Collapsed),
                }))
            }
            _ => Err(
                "a layout node is either `{ split, ratios, children }` or `{ tabs, active }`"
                    .into(),
            ),
        }
    }
}

impl From<Node> for RawNode {
    fn from(n: Node) -> Self {
        match n {
            Node::Split(s) => RawNode {
                split: Some(s.axis),
                studio: s.studio.filter(|_| s.is_studio_shape()),
                ratios: Some(s.ratios),
                children: Some(s.children),
                tabs: None,
                active: None,
                state: None,
            },
            Node::Tabs(t) => RawNode {
                split: None,
                ratios: None,
                children: None,
                active: (t.active != 0).then_some(t.active),
                state: t.collapsed.then_some(StackState::Collapsed),
                studio: None,
                tabs: Some(t.tabs),
            },
        }
    }
}

/// An axis-aligned rectangle in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// Left.
    pub x: f32,
    /// Top.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Rect {
    /// A rectangle at the origin.
    pub fn sized(w: f32, h: f32) -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            w,
            h,
        }
    }
}

/// A tab stack placed at an absolute rectangle.
#[derive(Clone, Debug, PartialEq)]
pub struct StackRect {
    /// Depth-first index of the stack.
    pub stack: usize,
    /// Where it is.
    pub rect: Rect,
    /// Its panels.
    pub tabs: Vec<PanelId>,
    /// Visible tab.
    pub active: usize,
    /// Collapsed: only its title row (or strip, in a horizontal split) is drawn.
    pub collapsed: bool,
}

/// A splitter handle between child `index` and `index + 1` of a split.
#[derive(Clone, Debug, PartialEq)]
pub struct Handle {
    /// Child indices from the root to the split.
    pub path: Vec<usize>,
    /// Index of the child before the handle.
    pub index: usize,
    /// Direction of the split.
    pub axis: Axis,
    /// Where the handle is.
    pub rect: Rect,
    /// Length of the split along its axis, minus handles.
    pub extent: f32,
}

/// A window's layout as absolute rectangles, for toolkits without recursive components.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Flat {
    /// Tab stacks in depth-first order.
    pub stacks: Vec<StackRect>,
    /// Splitter handles.
    pub handles: Vec<Handle>,
    /// The Transition rail of a studio split in studio mode (ADR-0064).
    pub rail: Option<(Rect, RailAt)>,
}

impl Node {
    /// An empty window (every panel closed).
    pub fn empty() -> Node {
        Node::Tabs(Tabs {
            tabs: Vec::new(),
            active: 0,
            collapsed: false,
        })
    }

    /// A tab stack.
    pub fn tabs(tabs: Vec<PanelId>) -> Node {
        Node::Tabs(Tabs {
            tabs,
            active: 0,
            collapsed: false,
        })
    }

    /// A split with the given shares (normalised to sum to 1).
    pub fn split(axis: Axis, parts: Vec<(f32, Node)>) -> Node {
        let sum: f32 = parts.iter().map(|(r, _)| *r).sum();
        let (ratios, children) = parts.into_iter().map(|(r, n)| (r / sum, n)).unzip();
        Node::Split(Split {
            axis,
            ratios,
            children,
            studio: None,
        })
    }

    /// A studio split (ADR-0064): `preview` and `program` side by side, the rail `at`.
    pub fn studio(preview: PanelId, program: PanelId, at: RailAt) -> Node {
        Node::Split(Split {
            axis: Axis::Horizontal,
            ratios: vec![0.5, 0.5],
            children: vec![Node::tabs(vec![preview]), Node::tabs(vec![program])],
            studio: Some(at),
        })
    }

    /// The studio split in this tree, if any.
    pub fn studio_split(&self) -> Option<&Split> {
        match self {
            Node::Split(s) if s.studio.is_some() && s.is_studio_shape() => Some(s),
            Node::Split(s) => s.children.iter().find_map(Node::studio_split),
            Node::Tabs(_) => None,
        }
    }

    /// The studio split in this tree, mutably.
    pub fn studio_split_mut(&mut self) -> Option<&mut Split> {
        let here = matches!(self, Node::Split(s) if s.studio.is_some() && s.is_studio_shape());
        match self {
            Node::Split(s) => {
                if here {
                    Some(s)
                } else {
                    s.children.iter_mut().find_map(Node::studio_split_mut)
                }
            }
            Node::Tabs(_) => None,
        }
    }

    /// Drops the studio flag of every split that no longer has a studio split's shape
    /// (a panel moved, closed or collapsed out of it, ADR-0064 decision 4).
    pub fn dissolve_broken_studio(&mut self) {
        if let Node::Split(s) = self {
            s.children.iter_mut().for_each(Node::dissolve_broken_studio);
            if s.studio.is_some() && !s.is_studio_shape() {
                s.studio = None;
            }
        }
    }

    /// True when no panel is in this tree.
    pub fn is_empty(&self) -> bool {
        match self {
            Node::Tabs(t) => t.tabs.is_empty(),
            Node::Split(s) => s.children.iter().all(Node::is_empty),
        }
    }

    /// Is there an empty tab stack anywhere except as the whole tree?
    pub fn has_empty_stack_below_root(&self) -> bool {
        match self {
            Node::Tabs(_) => false,
            Node::Split(s) => s.children.iter().any(|c| match c {
                Node::Tabs(t) => t.tabs.is_empty(),
                Node::Split(_) => c.has_empty_stack_below_root(),
            }),
        }
    }

    /// Depth of the deepest split (a tab stack has depth 0).
    pub fn depth(&self) -> usize {
        match self {
            Node::Tabs(_) => 0,
            Node::Split(s) => 1 + s.children.iter().map(Node::depth).max().unwrap_or(0),
        }
    }

    /// Every panel in the tree, depth first.
    pub fn panels(&self) -> Vec<&PanelId> {
        let mut out = Vec::new();
        self.visit_stacks(&mut |t| out.extend(t.tabs.iter()));
        out
    }

    /// The active panel of every stack that is not collapsed, depth first.
    pub fn visible(&self) -> Vec<&PanelId> {
        let mut out = Vec::new();
        self.visit_stacks(&mut |t| {
            if !t.collapsed {
                out.extend(t.tabs.get(t.active));
            }
        });
        out
    }

    /// Does the tree have a tab stack that is not collapsed? The centre must keep one
    /// (ADR-0049 decision 2).
    pub fn has_expanded_stack(&self) -> bool {
        let mut any = false;
        self.visit_stacks(&mut |t| any |= !t.collapsed);
        any
    }

    /// Number of tab stacks.
    pub fn stack_count(&self) -> usize {
        let mut n = 0;
        self.visit_stacks(&mut |_| n += 1);
        n
    }

    fn visit_stacks<'a>(&'a self, f: &mut impl FnMut(&'a Tabs)) {
        match self {
            Node::Tabs(t) => f(t),
            Node::Split(s) => s.children.iter().for_each(|c| c.visit_stacks(f)),
        }
    }

    /// The stack at depth-first index `index`.
    pub fn stack(&self, index: usize) -> Option<&Tabs> {
        let mut i = 0;
        let mut found = None;
        self.visit_stacks(&mut |t| {
            if i == index {
                found = Some(t);
            }
            i += 1;
        });
        found
    }

    /// The stack at depth-first index `index`, mutably.
    pub fn stack_mut(&mut self, index: usize) -> Option<&mut Tabs> {
        fn go<'a>(n: &'a mut Node, index: usize, i: &mut usize) -> Option<&'a mut Tabs> {
            match n {
                Node::Tabs(t) => {
                    let me = *i;
                    *i += 1;
                    (me == index).then_some(t)
                }
                Node::Split(s) => s.children.iter_mut().find_map(|c| go(c, index, i)),
            }
        }
        go(self, index, &mut 0)
    }

    /// Stack index and tab index of panel `id`.
    pub fn find(&self, id: &PanelId) -> Option<(usize, usize)> {
        let mut i = 0;
        let mut found = None;
        self.visit_stacks(&mut |t| {
            if found.is_none() {
                if let Some(tab) = t.tabs.iter().position(|p| p == id) {
                    found = Some((i, tab));
                }
            }
            i += 1;
        });
        found
    }

    /// Remove panel `id`; empty stacks are removed and single-child splits collapse.
    /// Returns whether the panel was here.
    pub fn remove(&mut self, id: &PanelId) -> bool {
        let Some((stack, tab)) = self.find(id) else {
            return false;
        };
        if let Some(t) = self.stack_mut(stack) {
            t.tabs.remove(tab);
            if t.active > tab || t.active >= t.tabs.len() {
                t.active = t.active.saturating_sub(1);
            }
        }
        self.prune();
        true
    }

    /// Add `id` as the active tab of stack `stack`, at `position` or at the end.
    /// Returns false when there is no such stack.
    pub fn insert_into_stack(
        &mut self,
        stack: usize,
        id: PanelId,
        position: Option<usize>,
    ) -> bool {
        let Some(t) = self.stack_mut(stack) else {
            return false;
        };
        let at = position.unwrap_or(t.tabs.len()).min(t.tabs.len());
        t.tabs.insert(at, id);
        t.active = at;
        true
    }

    /// Add `id` in a new stack along `edge` of the whole tree, taking [`EDGE_SHARE`].
    pub fn insert_at_edge(&mut self, id: PanelId, edge: Edge) {
        self.insert_node_at_edge(Node::tabs(vec![id]), edge);
    }

    /// Add `new` along `edge` of the whole tree, taking [`EDGE_SHARE`].
    pub fn insert_node_at_edge(&mut self, new: Node, edge: Edge) {
        if self.is_empty() {
            *self = new;
            return;
        }
        let (axis, first) = edge_axis(edge);
        match self {
            Node::Split(s) if s.axis == axis => {
                s.ratios.iter_mut().for_each(|r| *r *= 1.0 - EDGE_SHARE);
                if first {
                    s.ratios.insert(0, EDGE_SHARE);
                    s.children.insert(0, new);
                } else {
                    s.ratios.push(EDGE_SHARE);
                    s.children.push(new);
                }
            }
            _ => {
                let old = std::mem::replace(self, Node::empty());
                let parts = if first {
                    vec![(EDGE_SHARE, new), (1.0 - EDGE_SHARE, old)]
                } else {
                    vec![(1.0 - EDGE_SHARE, old), (EDGE_SHARE, new)]
                };
                *self = Node::split(axis, parts);
            }
        }
    }

    /// Remove the whole stack holding panel `id` and prune the tree. Returns the stack
    /// (expanded again) and its anchor: a panel of the neighbour it shared a split
    /// with, the one before it if there is one (ADR-0049 decision 2). The tree is
    /// left empty when the stack was its root.
    pub fn take_stack_of(&mut self, id: &PanelId) -> Option<(Tabs, Option<PanelId>)> {
        fn go(n: &mut Node, id: &PanelId) -> Option<(Tabs, Option<PanelId>)> {
            match n {
                Node::Tabs(t) if t.tabs.contains(id) => {
                    let mut taken = std::mem::replace(
                        t,
                        Tabs {
                            tabs: Vec::new(),
                            active: 0,
                            collapsed: false,
                        },
                    );
                    taken.collapsed = false;
                    Some((taken, None))
                }
                Node::Tabs(_) => None,
                Node::Split(s) => {
                    let (i, (taken, anchor)) = s
                        .children
                        .iter_mut()
                        .enumerate()
                        .find_map(|(i, c)| go(c, id).map(|r| (i, r)))?;
                    let anchor = anchor.or_else(|| {
                        let before = i
                            .checked_sub(1)
                            .and_then(|b| s.children[b].panels().last().copied());
                        let after = s
                            .children
                            .get(i + 1)
                            .and_then(|c| c.panels().first().copied());
                        before.or(after).cloned()
                    });
                    Some((taken, anchor))
                }
            }
        }
        let out = go(self, id)?;
        self.prune();
        Some(out)
    }

    /// Put `stack` back next to the stack holding `anchor`: after it in its parent
    /// split when that split runs along `axis`, else in a new split along `axis`.
    /// Returns false (and leaves the tree alone) when `anchor` is not here.
    pub fn insert_stack_beside(&mut self, anchor: &PanelId, stack: Tabs, axis: Axis) -> bool {
        fn go(n: &mut Node, anchor: &PanelId, stack: &mut Option<Tabs>, axis: Axis) -> bool {
            match n {
                Node::Tabs(t) if t.tabs.contains(anchor) => {
                    let old = std::mem::replace(n, Node::empty());
                    let new = Node::Tabs(stack.take().unwrap_or_else(|| Tabs {
                        tabs: Vec::new(),
                        active: 0,
                        collapsed: false,
                    }));
                    *n = Node::split(axis, vec![(0.5, old), (0.5, new)]);
                    true
                }
                Node::Tabs(_) => false,
                Node::Split(s) => {
                    let Some(i) = s.children.iter().position(|c| c.find(anchor).is_some()) else {
                        return false;
                    };
                    if s.axis == axis && matches!(s.children[i], Node::Tabs(_)) {
                        let share = 1.0 / (s.children.len() + 1) as f32;
                        s.ratios.iter_mut().for_each(|r| *r *= 1.0 - share);
                        s.ratios.insert(i + 1, share);
                        if let Some(t) = stack.take() {
                            s.children.insert(i + 1, Node::Tabs(t));
                        }
                        return true;
                    }
                    go(&mut s.children[i], anchor, stack, axis)
                }
            }
        }
        if stack.tabs.is_empty() {
            return false;
        }
        go(self, anchor, &mut Some(stack), axis)
    }

    /// Put `stack` on the `edge` side of stack number `index` (a drop on that side of
    /// the five-way guide, ADR-0049 decision 5). The two share the old stack's place
    /// half and half: inside its parent split when that split runs along the edge's
    /// axis, else in a new split in its place. Returns false (and leaves the tree
    /// alone) when there is no such stack or `stack` has no tabs.
    pub fn insert_beside(&mut self, index: usize, stack: Tabs, edge: Edge) -> bool {
        fn go(n: &mut Node, index: &mut usize, new: &mut Option<Tabs>, edge: Edge) -> bool {
            let (axis, first) = edge_axis(edge);
            match n {
                Node::Tabs(_) if *index == 0 => {
                    let Some(t) = new.take() else { return false };
                    let old = std::mem::replace(n, Node::empty());
                    let parts = if first {
                        vec![(0.5, Node::Tabs(t)), (0.5, old)]
                    } else {
                        vec![(0.5, old), (0.5, Node::Tabs(t))]
                    };
                    *n = Node::split(axis, parts);
                    true
                }
                Node::Tabs(_) => {
                    *index -= 1;
                    false
                }
                Node::Split(s) => {
                    for i in 0..s.children.len() {
                        if *index == 0 && s.axis == axis && matches!(s.children[i], Node::Tabs(_)) {
                            let Some(t) = new.take() else { return false };
                            let half = s.ratios[i] / 2.0;
                            s.ratios[i] = half;
                            let at = if first { i } else { i + 1 };
                            s.ratios.insert(at, half);
                            s.children.insert(at, Node::Tabs(t));
                            return true;
                        }
                        if go(&mut s.children[i], index, new, edge) {
                            return true;
                        }
                    }
                    false
                }
            }
        }
        if stack.tabs.is_empty() {
            return false;
        }
        go(self, &mut { index }, &mut Some(stack), edge)
    }

    /// Make tab `tab` of stack `stack` visible. Returns false when out of range.
    pub fn activate(&mut self, stack: usize, tab: usize) -> bool {
        match self.stack_mut(stack) {
            Some(t) if tab < t.tabs.len() => {
                t.active = tab;
                true
            }
            _ => false,
        }
    }

    /// Remove empty children, collapse single-child splits and renormalise ratios.
    pub fn prune(&mut self) {
        self.prune_splits();
        self.dissolve_broken_studio();
    }

    fn prune_splits(&mut self) {
        let Node::Split(s) = self else {
            return;
        };
        s.children.iter_mut().for_each(Node::prune_splits);
        let mut ratios = Vec::with_capacity(s.ratios.len());
        let mut children = Vec::with_capacity(s.children.len());
        for (c, r) in s.children.drain(..).zip(s.ratios.drain(..)) {
            if !c.is_empty() {
                children.push(c);
                ratios.push(r);
            }
        }
        match children.len() {
            0 => *self = Node::empty(),
            1 => {
                if let Some(only) = children.pop() {
                    *self = only;
                }
            }
            _ => {
                let sum: f32 = ratios.iter().sum();
                ratios.iter_mut().for_each(|r| *r /= sum);
                s.ratios = ratios;
                s.children = children;
            }
        }
    }

    /// Place the tree in `rect`: absolute stack rectangles and splitter handles.
    pub fn flatten(&self, rect: Rect) -> Flat {
        self.flatten_in(rect, false)
    }

    /// [`Node::flatten`] with the canvas in studio mode or not: outside it a studio
    /// split shows Preview alone; in it, both pictures and the rail (ADR-0064). Stack
    /// indices are the same either way.
    pub fn flatten_in(&self, rect: Rect, studio_mode: bool) -> Flat {
        let mut flat = Flat::default();
        let mut path = Vec::new();
        let mut next = 0;
        flatten_node(self, rect, studio_mode, &mut next, &mut path, &mut flat);
        flat
    }

    /// Move the splitter `handle` (from [`Node::flatten`]) by `delta` logical
    /// pixels. Neither neighbour gets smaller than [`MIN_PANE`] where the split
    /// is big enough. Returns false when the handle no longer matches the tree.
    pub fn resize(&mut self, handle: &Handle, delta: f32) -> bool {
        let mut node = self;
        for &i in &handle.path {
            match node {
                Node::Split(s) if i < s.children.len() => node = &mut s.children[i],
                _ => return false,
            }
        }
        let Node::Split(s) = node else {
            return false;
        };
        let i = handle.index;
        if s.axis != handle.axis || i + 1 >= s.ratios.len() || handle.extent <= 0.0 {
            return false;
        }
        let pair = s.ratios[i] + s.ratios[i + 1];
        let min = (MIN_PANE / handle.extent).min(pair / 2.0);
        let a = (s.ratios[i] + delta / handle.extent).clamp(min, pair - min);
        s.ratios[i] = a;
        s.ratios[i + 1] = pair - a;
        true
    }
}

fn is_collapsed(n: &Node) -> bool {
    matches!(n, Node::Tabs(t) if t.collapsed)
}

fn flatten_node(
    node: &Node,
    r: Rect,
    studio_mode: bool,
    next: &mut usize,
    path: &mut Vec<usize>,
    out: &mut Flat,
) {
    match node {
        Node::Tabs(t) => {
            out.stacks.push(StackRect {
                stack: *next,
                rect: r,
                tabs: t.tabs.clone(),
                active: t.active,
                collapsed: t.collapsed,
            });
            *next += 1;
        }
        Node::Split(s) if s.studio.is_some() && s.is_studio_shape() => {
            flatten_studio(s, r, studio_mode, next, path, out);
        }
        Node::Split(s) => {
            let n = s.children.len();
            let length = match s.axis {
                Axis::Horizontal => r.w,
                Axis::Vertical => r.h,
            };
            let usable = (length - HANDLE_THICKNESS * n.saturating_sub(1) as f32).max(0.0);
            // Collapsed stacks take their title row; the others share the rest by
            // ratio, so a collapsed stack keeps its ratio for when it expands.
            let collapsed = s.children.iter().filter(|c| is_collapsed(c)).count();
            let free = (usable - COLLAPSED_STACK * collapsed as f32).max(0.0);
            let open_share: f32 = s
                .children
                .iter()
                .zip(&s.ratios)
                .filter(|(c, _)| !is_collapsed(c))
                .map(|(_, r)| *r)
                .sum();
            let mut pos = 0.0;
            for (i, child) in s.children.iter().enumerate() {
                let len = if is_collapsed(child) {
                    COLLAPSED_STACK.min(usable)
                } else if open_share > 0.0 {
                    free * s.ratios.get(i).copied().unwrap_or(0.0) / open_share
                } else {
                    0.0
                };
                let along = |at: f32, len: f32| match s.axis {
                    Axis::Horizontal => Rect {
                        x: r.x + at,
                        y: r.y,
                        w: len,
                        h: r.h,
                    },
                    Axis::Vertical => Rect {
                        x: r.x,
                        y: r.y + at,
                        w: r.w,
                        h: len,
                    },
                };
                path.push(i);
                flatten_node(child, along(pos, len), studio_mode, next, path, out);
                path.pop();
                pos += len;
                if i + 1 < n {
                    // A collapsed neighbour has no size to drag; its divider stays put.
                    if !is_collapsed(child) && !is_collapsed(&s.children[i + 1]) {
                        out.handles.push(Handle {
                            path: path.clone(),
                            index: i,
                            axis: s.axis,
                            rect: along(pos, HANDLE_THICKNESS),
                            // Pixels per unit of ratio among the expanded children.
                            extent: if open_share > 0.0 {
                                free / open_share
                            } else {
                                0.0
                            },
                        });
                    }
                    pos += HANDLE_THICKNESS;
                }
            }
        }
    }
}

/// A studio split (ADR-0064): outside studio mode Preview fills it and Program is
/// hidden (its stack index is still counted); in studio mode the two pictures share it
/// by ratio around the rail, or above it.
fn flatten_studio(
    s: &Split,
    r: Rect,
    studio_mode: bool,
    next: &mut usize,
    path: &mut [usize],
    out: &mut Flat,
) {
    let stack = |n: &Node, rect: Rect, next: &mut usize, out: &mut Flat| {
        if let Node::Tabs(t) = n {
            out.stacks.push(StackRect {
                stack: *next,
                rect,
                tabs: t.tabs.clone(),
                active: t.active,
                collapsed: false,
            });
        }
        *next += 1;
    };
    let (Some(preview), Some(program)) = (s.children.first(), s.children.get(1)) else {
        return;
    };
    if !studio_mode {
        stack(preview, r, next, out);
        *next += 1;
        return;
    }
    let share = s.ratios.first().copied().unwrap_or(0.5).clamp(0.0, 1.0);
    // Too narrow for a column between two usable pictures: the rail goes below.
    let at = match s.studio {
        Some(RailAt::Between) if r.w >= RAIL_BETWEEN + 2.0 * MIN_PANE => RailAt::Between,
        _ => RailAt::Below,
    };
    match at {
        RailAt::Below => {
            let rail_h = RAIL_BELOW.min(r.h);
            let top_h = r.h - rail_h;
            let free = (r.w - HANDLE_THICKNESS).max(0.0);
            let a = free * share;
            stack(
                preview,
                Rect {
                    x: r.x,
                    y: r.y,
                    w: a,
                    h: top_h,
                },
                next,
                out,
            );
            out.handles.push(Handle {
                path: path.to_vec(),
                index: 0,
                axis: Axis::Horizontal,
                rect: Rect {
                    x: r.x + a,
                    y: r.y,
                    w: HANDLE_THICKNESS,
                    h: top_h,
                },
                extent: free,
            });
            let x = r.x + a + HANDLE_THICKNESS;
            stack(
                program,
                Rect {
                    x,
                    y: r.y,
                    w: free - a,
                    h: top_h,
                },
                next,
                out,
            );
            // A divider's gap above the rail, as between two stacks.
            let gap = HANDLE_THICKNESS.min(rail_h);
            let rail = Rect {
                x: r.x,
                y: r.y + top_h + gap,
                w: r.w,
                h: rail_h - gap,
            };
            out.rail = Some((rail, RailAt::Below));
        }
        RailAt::Between => {
            let rail_w = RAIL_BETWEEN.min(r.w);
            let free = (r.w - rail_w).max(0.0);
            let a = free * share;
            stack(
                preview,
                Rect {
                    x: r.x,
                    y: r.y,
                    w: a,
                    h: r.h,
                },
                next,
                out,
            );
            // A divider's gap on each side of the rail, as between two stacks.
            let gap = HANDLE_THICKNESS.min(rail_w / 2.0);
            let rail = Rect {
                x: r.x + a + gap,
                y: r.y,
                w: rail_w - 2.0 * gap,
                h: r.h,
            };
            out.rail = Some((rail, RailAt::Between));
            let x = r.x + a + rail_w;
            stack(
                program,
                Rect {
                    x,
                    y: r.y,
                    w: free - a,
                    h: r.h,
                },
                next,
                out,
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::panel::id;

    fn sample() -> Node {
        Node::split(
            Axis::Vertical,
            vec![
                (
                    0.7,
                    Node::split(
                        Axis::Horizontal,
                        vec![
                            (0.25, Node::tabs(vec![id("rbs.scenes"), id("rbs.sources")])),
                            (0.75, Node::tabs(vec![id("rbs.program")])),
                        ],
                    ),
                ),
                (0.3, Node::tabs(vec![id("rbs.mixer")])),
            ],
        )
    }

    fn one(p: &'static str) -> Tabs {
        Tabs {
            tabs: vec![id(p)],
            active: 0,
            collapsed: false,
        }
    }

    #[test]
    fn insert_beside_shares_the_stack_place() {
        // Along the parent split's axis: a sibling taking half of the stack's share.
        let mut n = sample();
        assert!(n.insert_beside(2, one("rbs.stats"), Edge::Bottom));
        let Node::Split(s) = &n else { panic!() };
        assert_eq!(s.children.len(), 3);
        assert_eq!(s.ratios, vec![0.7, 0.15, 0.15]);
        assert_eq!(n.find(&id("rbs.stats")), Some((3, 0)));
        // Across it: a new split in the stack's place, the new stack first on the left.
        let mut n = sample();
        assert!(n.insert_beside(2, one("rbs.stats"), Edge::Left));
        assert_eq!(n.depth(), 2);
        assert_eq!(n.find(&id("rbs.stats")), Some((2, 0)));
        assert_eq!(n.find(&id("rbs.mixer")), Some((3, 0)));
        // Inside the inner horizontal split, before the Program stack.
        let mut n = sample();
        assert!(n.insert_beside(1, one("rbs.stats"), Edge::Left));
        assert_eq!(n.find(&id("rbs.stats")), Some((1, 0)));
        assert_eq!(n.find(&id("rbs.program")), Some((2, 0)));
        // A root stack becomes a split.
        let mut n = Node::tabs(vec![id("rbs.program")]);
        assert!(n.insert_beside(0, one("rbs.stats"), Edge::Top));
        assert_eq!(n.find(&id("rbs.stats")), Some((0, 0)));
        // No such stack, or nothing to insert: unchanged.
        let mut n = sample();
        assert!(!n.insert_beside(3, one("rbs.stats"), Edge::Top));
        assert!(!n.insert_beside(
            0,
            Tabs {
                tabs: Vec::new(),
                active: 0,
                collapsed: false
            },
            Edge::Top
        ));
        assert_eq!(n, sample());
    }

    #[test]
    fn find_remove_and_prune() {
        let mut n = sample();
        assert_eq!(n.stack_count(), 3);
        assert_eq!(n.find(&id("rbs.sources")), Some((0, 1)));
        assert!(n.remove(&id("rbs.program")));
        assert!(!n.remove(&id("rbs.program")));
        // The horizontal split collapsed into its remaining stack.
        assert_eq!(n.stack_count(), 2);
        assert_eq!(n.depth(), 1);
        assert!(n.remove(&id("rbs.mixer")));
        assert!(matches!(n, Node::Tabs(_)));
        assert!(n.remove(&id("rbs.scenes")));
        assert!(n.remove(&id("rbs.sources")));
        assert!(n.is_empty());
    }

    #[test]
    fn active_tab_follows_removal() {
        let mut n = Node::tabs(vec![id("rbs.scenes"), id("rbs.sources"), id("rbs.stats")]);
        assert!(n.activate(0, 2));
        n.remove(&id("rbs.scenes"));
        assert_eq!(n.visible(), vec![&id("rbs.stats")]);
        n.remove(&id("rbs.stats"));
        assert_eq!(n.visible(), vec![&id("rbs.sources")]);
        assert!(!n.activate(0, 5));
    }

    #[test]
    fn edge_insert_wraps_or_extends() {
        let mut n = sample();
        n.insert_at_edge(id("rbs.stats"), Edge::Bottom);
        let Node::Split(s) = &n else { panic!() };
        assert_eq!(s.children.len(), 3);
        assert!((s.ratios.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        assert_eq!(s.ratios[2], EDGE_SHARE);
        n.insert_at_edge(id("rbs.outputs"), Edge::Left);
        let Node::Split(s) = &n else { panic!() };
        assert_eq!(s.axis, Axis::Horizontal);
        assert_eq!(n.find(&id("rbs.outputs")), Some((0, 0)));
        let mut e = Node::empty();
        e.insert_at_edge(id("rbs.mixer"), Edge::Top);
        assert_eq!(e, Node::tabs(vec![id("rbs.mixer")]));
    }

    #[test]
    fn flatten_tiles_the_rect() {
        let flat = sample().flatten(Rect::sized(1006.0, 806.0));
        assert_eq!(flat.stacks.len(), 3);
        assert_eq!(flat.handles.len(), 2);
        let area: f32 = flat.stacks.iter().map(|s| s.rect.w * s.rect.h).sum::<f32>()
            + flat
                .handles
                .iter()
                .map(|h| h.rect.w * h.rect.h)
                .sum::<f32>();
        assert!((area - 1006.0 * 806.0).abs() < 1.0, "{area}");
        assert_eq!(flat.stacks[2].rect.y, 0.7 * 800.0 + HANDLE_THICKNESS);
    }

    #[test]
    fn resize_clamps_and_keeps_sum() {
        let mut n = sample();
        let flat = n.flatten(Rect::sized(800.0, 606.0));
        let h = flat
            .handles
            .iter()
            .find(|h| h.axis == Axis::Vertical)
            .unwrap()
            .clone();
        assert!(n.resize(&h, -60.0));
        let Node::Split(s) = &n else { panic!() };
        assert!((s.ratios[0] - 0.6).abs() < 1e-4, "{:?}", s.ratios);
        assert!(n.resize(&h, 10_000.0));
        let Node::Split(s) = &n else { panic!() };
        assert!((s.ratios[1] - MIN_PANE / 600.0).abs() < 1e-4);
        assert!((s.ratios.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        let mut stale = h.clone();
        stale.path = vec![7];
        assert!(!n.resize(&stale, 1.0));
    }

    #[test]
    fn file_shape_round_trips_and_validates() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct W {
            layout: Node,
        }
        let w = W { layout: sample() };
        let s = toml::to_string(&w).unwrap();
        assert_eq!(toml::from_str::<W>(&s).unwrap(), w, "{s}");
        for bad in [
            "layout = { split = \"vertical\", ratios = [1.0], children = [{ tabs = [\"rbs.a\"] }] }",
            "layout = { split = \"vertical\", ratios = [0.5, 0.2], children = [{ tabs = [\"rbs.a\"] }, { tabs = [\"rbs.b\"] }] }",
            "layout = { split = \"vertical\", ratios = [1.5, -0.5], children = [{ tabs = [\"rbs.a\"] }, { tabs = [\"rbs.b\"] }] }",
            "layout = { tabs = [\"rbs.a\"], active = 1 }",
            "layout = { tabs = [\"rbs.a\"], split = \"vertical\" }",
            "layout = { tabs = [\"rbs.a\"], colour = 1 }",
            "layout = { tabs = [\"Not An Id\"] }",
        ] {
            assert!(toml::from_str::<W>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_studio_split_shows_program_and_the_rail_only_in_studio_mode() {
        let n = Node::studio(id("rbs.preview"), id("rbs.program"), RailAt::Between);
        let r = Rect::sized(1000.0, 400.0);
        // Outside studio mode: Preview fills it; Program is hidden but keeps index 1.
        let off = n.flatten_in(r, false);
        assert_eq!(off.stacks.len(), 1);
        assert_eq!(off.stacks[0].rect, r);
        assert!(off.rail.is_none() && off.handles.is_empty());
        // Between: Preview | rail | Program, the rail in place of the divider.
        let on = n.flatten_in(r, true);
        assert_eq!(on.stacks.len(), 2);
        assert_eq!(on.stacks[1].stack, 1);
        let (rail, at) = on.rail.unwrap();
        assert_eq!(at, RailAt::Between);
        assert_eq!(rail.w, RAIL_BETWEEN - 2.0 * HANDLE_THICKNESS);
        assert_eq!(on.stacks[0].rect.w, on.stacks[1].rect.w);
        assert_eq!(rail.x, on.stacks[0].rect.w + HANDLE_THICKNESS);
        assert_eq!(on.stacks[1].rect.x, on.stacks[0].rect.w + RAIL_BETWEEN);
        assert!(on.handles.is_empty());
        // Below: both pictures over a full-width rail, a divider between them.
        let mut below = n.clone();
        if let Node::Split(s) = &mut below {
            s.studio = Some(RailAt::Below);
        }
        let on = below.flatten_in(r, true);
        let (rail, _) = on.rail.unwrap();
        assert_eq!(
            (rail.y, rail.w, rail.h),
            (
                400.0 - RAIL_BELOW + HANDLE_THICKNESS,
                1000.0,
                RAIL_BELOW - HANDLE_THICKNESS
            )
        );
        assert_eq!(on.stacks[0].rect.h, 400.0 - RAIL_BELOW);
        assert_eq!(on.handles.len(), 1);
        assert!(below.resize(&on.handles[0], 50.0));
        // Too narrow for a column between two usable pictures: below instead.
        let narrow = n.flatten_in(
            Rect::sized(RAIL_BETWEEN + 2.0 * MIN_PANE - 1.0, 400.0),
            true,
        );
        assert_eq!(narrow.rail.unwrap().1, RailAt::Below);
    }

    #[test]
    fn stack_indices_after_a_studio_split_do_not_depend_on_studio_mode() {
        let n = Node::split(
            Axis::Vertical,
            vec![
                (
                    0.7,
                    Node::studio(id("rbs.preview"), id("rbs.program"), RailAt::Between),
                ),
                (0.3, Node::tabs(vec![id("rbs.mixer")])),
            ],
        );
        let r = Rect::sized(800.0, 600.0);
        for on in [false, true] {
            let f = n.flatten_in(r, on);
            let mixer = f
                .stacks
                .iter()
                .find(|s| s.tabs[0] == id("rbs.mixer"))
                .unwrap();
            assert_eq!(mixer.stack, 2);
            assert_eq!(n.stack(mixer.stack).unwrap().tabs, mixer.tabs);
        }
    }

    #[test]
    fn a_studio_split_keeps_its_shape_or_dissolves() {
        let text = r#"split = "horizontal"
studio = "below"
ratios = [0.5, 0.5]
children = [{ tabs = ["rbs.preview"] }, { tabs = ["rbs.program"] }]
"#;
        let n: Node = toml::from_str(text).unwrap();
        assert_eq!(n.studio_split().unwrap().studio, Some(RailAt::Below));
        let back = toml::to_string(&n).unwrap();
        assert!(back.contains("studio = \"below\""), "{back}");
        // Any other shape is refused in a file.
        let bad = text.replace("rbs.program", "rbs.mixer");
        assert!(toml::from_str::<Node>(&bad).is_err());
        let bad = text.replace("\"rbs.preview\"", "\"rbs.preview\", \"rbs.scenes\"");
        assert!(toml::from_str::<Node>(&bad).is_err());
        let bad = text.replace("rbs.program", "rbs.program?canvas=vertical");
        assert!(toml::from_str::<Node>(&bad).is_err());
        // A panel added to one of its stacks dissolves it; it is never written broken.
        let mut n = n;
        n.insert_into_stack(0, id("rbs.scenes"), None);
        assert!(n.studio_split().is_none());
        assert!(!toml::to_string(&n).unwrap().contains("studio"));
        n.dissolve_broken_studio();
        let Node::Split(s) = &n else { panic!() };
        assert_eq!(s.studio, None);
    }
}
