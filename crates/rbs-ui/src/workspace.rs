//! Workspace files: a saved arrangement of panels, floating windows and projectors.
//!
//! Spec: `docs/specs/ui-design-system.md`, "Workspace file" (format 2, ADR-0049). A
//! workspace holds layout only, never scenes, sources or output settings (glossary).
//!
//! The main window is a centre and three areas (left, right, bottom), each a tree of
//! tab stacks, plus groups of auto-hidden panels on its edges. Tab stacks of the main
//! window are numbered depth first through the centre, then the left, right and
//! bottom areas, so a stack index means the same thing to every operation.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::command::CommandId;
use crate::layout::{Edge, MAX_DEPTH, Node, Tabs};
use crate::panel::PanelId;

/// The workspace file format this crate writes. Format 1 files are read and
/// migrated in memory ([`Workspace::from_toml`]).
pub const FORMAT: u32 = 2;

/// Longest workspace name in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// Largest size in a file (areas, edge groups), in logical pixels.
pub const MAX_PIXELS: u32 = 16384;

/// Most auto-hide edge groups in a workspace.
pub const MAX_EDGE_GROUPS: usize = 16;

/// Most panels in one auto-hide edge group.
pub const MAX_GROUP_TABS: usize = 32;

/// Most toolbar items.
pub const MAX_TOOLBAR_ITEMS: usize = 64;

/// Default width of an auto-hide flyout, in logical pixels (spec, "Size limits").
pub const DEFAULT_FLYOUT: u32 = 320;

/// Where a window was, so it can be placed on the same display again.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    /// Stable display fingerprint derived from EDID, for example `edid:DEL-41A3-7H2K9`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// Connector name, for example `DP-2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector: Option<String>,
    /// Display size in physical pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<[u32; 2]>,
    /// Display scale factor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f32>,
    /// Window rectangle `[x, y, w, h]` in logical pixels relative to the display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<[f32; 4]>,
    /// Full screen on that display.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fullscreen: bool,
}

/// One of the three areas around the main window's centre, and the edges auto-hidden
/// panels sit on (there are no top edge tabs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// The left area and edge.
    Left,
    /// The right area and edge.
    Right,
    /// The bottom area and edge.
    Bottom,
}

impl Side {
    /// Every side, in stack numbering and file order.
    pub const ALL: [Side; 3] = [Side::Left, Side::Right, Side::Bottom];

    /// The area's default size: width for left and right, height for bottom.
    pub fn default_size(self) -> u32 {
        match self {
            Side::Left | Side::Right => 260,
            Side::Bottom => 200,
        }
    }
}

/// An area of the main window: a tree of tab stacks with a size that stays when the
/// window is resized (ADR-0049 decision 1).
#[derive(Clone, Debug, PartialEq)]
pub struct Area {
    /// Width (left, right) or height (bottom) in logical pixels, clamped to the area's
    /// limits when laid out. Kept while the area is hidden or empty.
    pub size: u32,
    /// Shown. A hidden area keeps its contents and size.
    pub visible: bool,
    /// Its panels; `None` when empty. Never holds an empty tab stack.
    pub layout: Option<Node>,
}

impl Area {
    /// An empty, visible area of the default size.
    pub fn empty(side: Side) -> Area {
        Area {
            size: side.default_size(),
            visible: true,
            layout: None,
        }
    }

    /// Is the area visible and holding at least one panel?
    pub fn shows(&self) -> bool {
        self.visible && self.layout.is_some()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArea {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u32>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    layout: Option<Node>,
}

fn yes() -> bool {
    true
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if signature"
)]
fn is_true(b: &bool) -> bool {
    *b
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if signature"
)]
fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl RawArea {
    fn into_area(self, side: Side) -> Area {
        Area {
            size: self.size.unwrap_or(side.default_size()),
            visible: self.visible,
            layout: self.layout,
        }
    }

    /// `None` for an area the file does not need to mention.
    fn from_area(a: Area, side: Side) -> Option<RawArea> {
        (a != Area::empty(side)).then_some(RawArea {
            size: Some(a.size),
            visible: a.visible,
            layout: a.layout,
        })
    }
}

/// Auto-hidden panels on one edge of the main window (ADR-0049 decision 2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeGroup {
    /// The edge its tabs are on.
    pub edge: Side,
    /// Its panels, 1 to [`MAX_GROUP_TABS`].
    pub tabs: Vec<PanelId>,
    /// Index of the tab shown when the group slides out.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub active: usize,
    /// Flyout width (or height on the bottom edge) in logical pixels, clamped to the
    /// flyout limits when shown.
    #[serde(default = "default_flyout")]
    pub size: u32,
    /// The panel it shared a stack or split with when it was auto-hidden; pinning docks
    /// it next to that panel when it is docked in the main window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<PanelId>,
}

fn default_flyout() -> u32 {
    DEFAULT_FLYOUT
}

/// The main window's content: the centre, three areas and the auto-hidden panels.
#[derive(Clone, Debug, PartialEq)]
pub struct MainLayout {
    /// The centre. The only tree that may be one empty stack (every panel closed), and
    /// it keeps at least one stack that is not collapsed.
    pub centre: Node,
    /// The left area.
    pub left: Area,
    /// The right area.
    pub right: Area,
    /// The bottom area, spanning the width under the others.
    pub bottom: Area,
    /// Auto-hidden panels, in file order (drawn in this order on each edge).
    pub auto_hide: Vec<EdgeGroup>,
}

impl MainLayout {
    /// `centre` with empty, visible areas and nothing auto-hidden: what a format 1
    /// main window becomes.
    pub fn new(centre: Node) -> MainLayout {
        MainLayout {
            centre,
            left: Area::empty(Side::Left),
            right: Area::empty(Side::Right),
            bottom: Area::empty(Side::Bottom),
            auto_hide: Vec::new(),
        }
    }

    /// The area on `side`.
    pub fn area(&self, side: Side) -> &Area {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
            Side::Bottom => &self.bottom,
        }
    }

    /// The area on `side`, mutably.
    pub fn area_mut(&mut self, side: Side) -> &mut Area {
        match side {
            Side::Left => &mut self.left,
            Side::Right => &mut self.right,
            Side::Bottom => &mut self.bottom,
        }
    }

    /// The edge group holding panel `id`.
    pub fn group_of(&self, id: &PanelId) -> Option<usize> {
        self.auto_hide.iter().position(|g| g.tabs.contains(id))
    }
}

/// How toolbar items are drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolbarStyle {
    /// Icons only, titles as tooltips.
    Icons,
    /// Icons with their titles.
    #[default]
    IconsText,
}

/// One toolbar item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolbarItem {
    /// A command registry id (ADR-0050) in canonical form ([`CommandId`]).
    Command(String),
    /// `|`: a separator.
    Separator,
    /// `~`: the flexible space; at most one.
    Space,
}

/// The workspace's toolbar (ADR-0050). Unknown command ids are kept; the shell shows
/// them as disabled placeholders.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawToolbar", into = "RawToolbar")]
pub struct Toolbar {
    /// How items are drawn.
    pub style: ToolbarStyle,
    /// Shown. Hidden, the row stays as a slim strip with the live indicator.
    pub visible: bool,
    /// Items in order, at most [`MAX_TOOLBAR_ITEMS`]; no leading, trailing or repeated
    /// separators.
    pub items: Vec<ToolbarItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToolbar {
    #[serde(default)]
    style: ToolbarStyle,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    visible: bool,
    #[serde(default)]
    items: Vec<String>,
}

impl TryFrom<RawToolbar> for Toolbar {
    type Error = String;

    fn try_from(r: RawToolbar) -> Result<Self, String> {
        if r.items.len() > MAX_TOOLBAR_ITEMS {
            return Err(format!("a toolbar has at most {MAX_TOOLBAR_ITEMS} items"));
        }
        let mut items: Vec<ToolbarItem> = Vec::with_capacity(r.items.len());
        for text in r.items {
            let item = match text.as_str() {
                "|" => ToolbarItem::Separator,
                "~" => ToolbarItem::Space,
                _ => {
                    let id = CommandId::parse(&text)
                        .map_err(|e| format!("toolbar item {text:?}: {e}"))?;
                    // Only the grammar is checked: an unknown id (a missing plugin) or
                    // one whose parameters its entry does not accept is kept and shown
                    // disabled with the reason, so a registry change never breaks a file.
                    ToolbarItem::Command(id.to_string())
                }
            };
            if item == ToolbarItem::Space && items.contains(&ToolbarItem::Space) {
                return Err("a toolbar has at most one flexible space `~`".into());
            }
            if matches!(&item, ToolbarItem::Command(_)) && items.contains(&item) {
                return Err(format!("toolbar item {text:?} appears more than once"));
            }
            items.push(item);
        }
        Ok(Toolbar {
            style: r.style,
            visible: r.visible,
            items: tidy_separators(items),
        })
    }
}

/// Leading, trailing and repeated separators removed (spec, "Toolbar").
fn tidy_separators(items: Vec<ToolbarItem>) -> Vec<ToolbarItem> {
    let mut out: Vec<ToolbarItem> = Vec::with_capacity(items.len());
    for item in items {
        let after_separator = matches!(out.last(), None | Some(ToolbarItem::Separator));
        if item == ToolbarItem::Separator && after_separator {
            continue;
        }
        out.push(item);
    }
    if out.last() == Some(&ToolbarItem::Separator) {
        out.pop();
    }
    out
}

impl From<Toolbar> for RawToolbar {
    fn from(t: Toolbar) -> Self {
        RawToolbar {
            style: t.style,
            visible: t.visible,
            items: t
                .items
                .into_iter()
                .map(|i| match i {
                    ToolbarItem::Command(id) => id,
                    ToolbarItem::Separator => "|".to_owned(),
                    ToolbarItem::Space => "~".to_owned(),
                })
                .collect(),
        }
    }
}

/// What a window shows.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowContent {
    /// The main window. Exactly one per workspace, listed first.
    Main(Box<MainLayout>),
    /// A floating panel window, with its own layout.
    Floating(Node),
    /// A projector: a chromeless view of a canvas, scene or source, for example
    /// `program?canvas=vertical`. Interpreted by the shell.
    Projector(String),
}

/// One window of a workspace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawWindow", into = "RawWindow")]
pub struct Window {
    /// Kind and content.
    pub content: WindowContent,
    /// Last known placement, if any.
    pub placement: Option<Placement>,
}

impl Window {
    /// The main tree: the centre of the main window, the layout of a floating window.
    pub fn layout(&self) -> Option<&Node> {
        match &self.content {
            WindowContent::Main(m) => Some(&m.centre),
            WindowContent::Floating(n) => Some(n),
            WindowContent::Projector(_) => None,
        }
    }

    /// The main tree, mutably.
    pub fn layout_mut(&mut self) -> Option<&mut Node> {
        match &mut self.content {
            WindowContent::Main(m) => Some(&mut m.centre),
            WindowContent::Floating(n) => Some(n),
            WindowContent::Projector(_) => None,
        }
    }

    /// Every tree of the window in stack numbering order: the main window's centre,
    /// then its left, right and bottom areas that hold panels; a floating window's
    /// layout.
    pub fn trees(&self) -> Vec<&Node> {
        match &self.content {
            WindowContent::Main(m) => std::iter::once(&m.centre)
                .chain(Side::ALL.iter().filter_map(|s| m.area(*s).layout.as_ref()))
                .collect(),
            WindowContent::Floating(n) => vec![n],
            WindowContent::Projector(_) => Vec::new(),
        }
    }

    fn trees_mut(&mut self) -> Vec<&mut Node> {
        match &mut self.content {
            WindowContent::Main(m) => std::iter::once(&mut m.centre)
                .chain(
                    [&mut m.left, &mut m.right, &mut m.bottom]
                        .into_iter()
                        .filter_map(|a| a.layout.as_mut()),
                )
                .collect(),
            WindowContent::Floating(n) => vec![n],
            WindowContent::Projector(_) => Vec::new(),
        }
    }

    /// The main window's layout.
    pub fn main(&self) -> Option<&MainLayout> {
        match &self.content {
            WindowContent::Main(m) => Some(m.as_ref()),
            _ => None,
        }
    }

    /// The main window's layout, mutably.
    pub fn main_mut(&mut self) -> Option<&mut MainLayout> {
        match &mut self.content {
            WindowContent::Main(m) => Some(m.as_mut()),
            _ => None,
        }
    }

    /// The window laid out in `content`: every tab stack shown and every divider.
    pub fn flatten(&self, content: crate::layout::Rect) -> crate::frame::WindowFlat {
        crate::frame::flatten_window(self, content)
    }

    /// Move divider `handle` of [`Window::flatten`] in `content` by `delta` logical
    /// pixels. Returns false when there is no such divider.
    pub fn resize(&mut self, content: crate::layout::Rect, handle: usize, delta: f32) -> bool {
        use crate::frame::HandleTarget;
        let Some(h) = self.flatten(content).handles.into_iter().nth(handle) else {
            return false;
        };
        match h.target {
            HandleTarget::Tree(t) => self
                .trees_mut()
                .into_iter()
                .nth(t)
                .is_some_and(|tree| tree.resize(&h.handle, delta)),
            HandleTarget::Area(side) => match self.main_mut() {
                Some(m) => {
                    let size = crate::frame::resized_area(m, content, side, delta);
                    m.area_mut(side).size = size;
                    true
                }
                None => false,
            },
        }
    }

    /// Is this the main window?
    pub fn is_main(&self) -> bool {
        matches!(self.content, WindowContent::Main(_))
    }

    /// Number of tab stacks in every tree of the window.
    pub fn stack_count(&self) -> usize {
        self.trees().iter().map(|t| t.stack_count()).sum()
    }

    /// The tree and its own stack index for window stack `stack`.
    fn locate(&self, stack: usize) -> Option<(usize, usize)> {
        let mut first = 0;
        for (i, t) in self.trees().iter().enumerate() {
            let n = t.stack_count();
            if stack < first + n {
                return Some((i, stack - first));
            }
            first += n;
        }
        None
    }

    /// The tab stack at window stack index `stack`.
    pub fn stack(&self, stack: usize) -> Option<&Tabs> {
        let (tree, local) = self.locate(stack)?;
        self.trees().get(tree)?.stack(local)
    }

    /// Make tab `tab` of stack `stack` the visible one.
    pub fn activate(&mut self, stack: usize, tab: usize) -> bool {
        let Some((tree, local)) = self.locate(stack) else {
            return false;
        };
        self.trees_mut()
            .into_iter()
            .nth(tree)
            .is_some_and(|t| t.activate(local, tab))
    }

    /// Window stack index and tab index of panel `id` in the window's trees.
    pub fn find(&self, id: &PanelId) -> Option<(usize, usize)> {
        let mut first = 0;
        for t in self.trees() {
            if let Some((s, tab)) = t.find(id) {
                return Some((first + s, tab));
            }
            first += t.stack_count();
        }
        None
    }

    fn insert_into_stack(&mut self, stack: usize, id: PanelId) -> bool {
        let Some((tree, local)) = self.locate(stack) else {
            return false;
        };
        self.trees_mut()
            .into_iter()
            .nth(tree)
            .is_some_and(|t| t.insert_into_stack(local, id, None))
    }

    /// Remove panel `id` from the window's trees and edge groups. An area left
    /// without panels becomes empty; an edge group left without panels goes away.
    fn remove(&mut self, id: &PanelId) -> bool {
        match &mut self.content {
            WindowContent::Main(m) => {
                if m.centre.remove(id) {
                    return true;
                }
                for a in [&mut m.left, &mut m.right, &mut m.bottom] {
                    if let Some(l) = &mut a.layout {
                        if l.remove(id) {
                            if l.is_empty() {
                                a.layout = None;
                            }
                            return true;
                        }
                    }
                }
                if let Some(g) = m.group_of(id) {
                    let group = &mut m.auto_hide[g];
                    if let Some(i) = group.tabs.iter().position(|p| p == id) {
                        group.tabs.remove(i);
                        if group.active > i || group.active >= group.tabs.len() {
                            group.active = group.active.saturating_sub(1);
                        }
                    }
                    if group.tabs.is_empty() {
                        m.auto_hide.remove(g);
                    }
                    return true;
                }
                false
            }
            WindowContent::Floating(n) => n.remove(id),
            WindowContent::Projector(_) => false,
        }
    }

    /// Does removing `id` remove its tab stack (and so renumber later stacks)? Only
    /// the centre keeps its last stack, empty.
    fn removal_drops_stack(&self, id: &PanelId) -> bool {
        let Some((stack, _)) = self.find(id) else {
            return false;
        };
        let lone = self.stack(stack).is_some_and(|t| t.tabs.len() == 1);
        let centre_root = matches!(&self.content, WindowContent::Main(m) if matches!(m.centre, Node::Tabs(_)) && stack == 0);
        lone && !centre_root
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawKind {
    Main,
    Floating,
    Projector,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWindow {
    kind: RawKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    layout: Option<Node>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    centre: Option<Node>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    left: Option<RawArea>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    right: Option<RawArea>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bottom: Option<RawArea>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    auto_hide: Vec<EdgeGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    placement: Option<Placement>,
}

impl TryFrom<RawWindow> for Window {
    type Error = String;

    fn try_from(r: RawWindow) -> Result<Self, String> {
        let has_areas =
            r.left.is_some() || r.right.is_some() || r.bottom.is_some() || !r.auto_hide.is_empty();
        let content = match (r.kind, r.layout, r.centre, r.content) {
            (RawKind::Main, None, Some(centre), None) => {
                let mut m = MainLayout::new(centre);
                m.left = r.left.map_or(m.left, |a| a.into_area(Side::Left));
                m.right = r.right.map_or(m.right, |a| a.into_area(Side::Right));
                m.bottom = r.bottom.map_or(m.bottom, |a| a.into_area(Side::Bottom));
                m.auto_hide = r.auto_hide;
                WindowContent::Main(Box::new(m))
            }
            (RawKind::Main, _, _, _) => {
                return Err("the main window has a `centre` and no `layout` or `content`".into());
            }
            _ if has_areas => {
                return Err(
                    "only the main window has `left`, `right`, `bottom` and `auto_hide`".into(),
                );
            }
            (RawKind::Floating, Some(l), None, None) if !l.is_empty() => WindowContent::Floating(l),
            (RawKind::Floating, Some(_), None, None) => {
                return Err("a floating window must hold at least one panel".into());
            }
            (RawKind::Projector, None, None, Some(c)) if !c.trim().is_empty() => {
                WindowContent::Projector(c)
            }
            (RawKind::Floating, _, _, _) => {
                return Err("a floating window has a `layout` and no `centre` or `content`".into());
            }
            (RawKind::Projector, _, _, _) => {
                return Err("a projector has a `content` and no `layout`".into());
            }
        };
        Ok(Window {
            content,
            placement: r.placement,
        })
    }
}

impl From<Window> for RawWindow {
    fn from(w: Window) -> Self {
        let mut raw = RawWindow {
            kind: RawKind::Main,
            layout: None,
            centre: None,
            left: None,
            right: None,
            bottom: None,
            auto_hide: Vec::new(),
            content: None,
            placement: w.placement,
        };
        match w.content {
            WindowContent::Main(m) => {
                let m = *m;
                raw.centre = Some(m.centre);
                raw.left = RawArea::from_area(m.left, Side::Left);
                raw.right = RawArea::from_area(m.right, Side::Right);
                raw.bottom = RawArea::from_area(m.bottom, Side::Bottom);
                raw.auto_hide = m.auto_hide;
            }
            WindowContent::Floating(l) => {
                raw.kind = RawKind::Floating;
                raw.layout = Some(l);
            }
            WindowContent::Projector(c) => {
                raw.kind = RawKind::Projector;
                raw.content = Some(c);
            }
        }
        raw
    }
}

/// A workspace.
///
/// Invariants, checked by [`Workspace::validate`] (called by [`Workspace::from_toml`]):
/// exactly one main window and it is first; a panel id appears at most once in the
/// whole workspace (trees and edge groups); only the main window's centre may be an
/// empty stack, and it keeps a stack that is not collapsed; areas hold no empty
/// stacks; splits are at most [`MAX_DEPTH`] deep; sizes and edge groups are within
/// their limits. The operations below keep all of them except the depth limit, which
/// repeated edge moves can exceed; [`crate::LayoutSession::apply`] validates every
/// result and refuses such a change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    /// File format version.
    pub format: u32,
    /// Display name, unique among the user's workspaces (case-insensitive).
    pub name: String,
    /// Pointer moves, resizes and close buttons are blocked. Commands still work.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub locked: bool,
    /// The toolbar; `None` uses the workspace's built-in default (or Edit's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolbar: Option<Toolbar>,
    /// Windows; the first is the main window.
    pub windows: Vec<Window>,
}

/// Errors from reading or changing a workspace.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceError {
    /// The text is not a valid workspace file.
    Parse(String),
    /// Written by a newer RBS.
    NewerFormat(u32),
    /// The file breaks a rule that the parser cannot express.
    Invalid(String),
    /// The panel is not open.
    NotOpen(PanelId),
    /// No such window or stack.
    NoTarget,
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkspaceError::Parse(e) => write!(f, "invalid workspace file: {e}"),
            WorkspaceError::NewerFormat(v) => write!(
                f,
                "workspace format {v} was written by a newer RBS (this one reads up to {FORMAT})"
            ),
            WorkspaceError::Invalid(e) => write!(f, "invalid workspace: {e}"),
            WorkspaceError::NotOpen(id) => write!(f, "panel {id} is not open"),
            WorkspaceError::NoTarget => write!(f, "no such window or tab stack"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

/// Are two workspace names the same? Names are unique ignoring case, using
/// Unicode lowercase (so "Çanlı" and "çanlı" are the same name).
pub fn same_name(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

/// Check a workspace name: 1 to 64 characters, not only spaces, no control characters.
pub fn validate_name(name: &str) -> Result<(), WorkspaceError> {
    let n = name.chars().count();
    if name.trim().is_empty() || n > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(WorkspaceError::Invalid(format!(
            "a workspace name has 1 to {MAX_NAME_CHARS} characters and no control characters"
        )));
    }
    Ok(())
}

/// The format a workspace file declares, without reading the rest. `None` when the
/// text is not TOML or has no `format`.
pub fn file_format(text: &str) -> Option<u32> {
    #[derive(Deserialize)]
    struct Version {
        format: Option<u32>,
    }
    toml::from_str::<Version>(text).ok()?.format
}

/// Keys format 2 added; a format 1 file using them is invalid.
const FORMAT_2_KEYS: [&str; 6] = ["centre", "left", "right", "bottom", "auto_hide", "toolbar"];

/// A format 1 file as format 2 data: the main window's `layout` becomes its `centre`
/// (areas empty and visible, nothing collapsed or auto-hidden, default toolbar).
fn migrate_1(text: &str) -> Result<toml::Table, WorkspaceError> {
    fn refuse_new_keys(v: &toml::Value) -> Result<(), WorkspaceError> {
        match v {
            toml::Value::Table(t) => {
                if let Some(k) = t
                    .keys()
                    .find(|k| FORMAT_2_KEYS.contains(&k.as_str()) || k.as_str() == "state")
                {
                    return Err(WorkspaceError::Parse(format!(
                        "unknown field `{k}` in a format 1 file"
                    )));
                }
                t.values().try_for_each(refuse_new_keys)
            }
            toml::Value::Array(a) => a.iter().try_for_each(refuse_new_keys),
            _ => Ok(()),
        }
    }
    let mut t: toml::Table =
        toml::from_str(text).map_err(|e| WorkspaceError::Parse(e.to_string()))?;
    refuse_new_keys(&toml::Value::Table(t.clone()))?;
    t.insert("format".into(), toml::Value::Integer(i64::from(FORMAT)));
    if let Some(toml::Value::Array(windows)) = t.get_mut("windows") {
        for w in windows {
            let toml::Value::Table(w) = w else { continue };
            if w.get("kind").and_then(toml::Value::as_str) == Some("main") {
                if let Some(layout) = w.remove("layout") {
                    w.insert("centre".into(), layout);
                }
            }
        }
    }
    Ok(t)
}

impl Workspace {
    /// A workspace with one main window holding `centre`, areas empty.
    pub fn new(name: &str, centre: Node) -> Workspace {
        Workspace::with_main(name, MainLayout::new(centre))
    }

    /// A workspace with one main window.
    pub fn with_main(name: &str, main: MainLayout) -> Workspace {
        Workspace {
            format: FORMAT,
            name: name.to_owned(),
            locked: false,
            toolbar: None,
            windows: vec![Window {
                content: WindowContent::Main(Box::new(main)),
                placement: None,
            }],
        }
    }

    /// Parse and validate a workspace file. Format 1 is migrated in memory: the main
    /// window's layout becomes the centre (ADR-0049 decision 6).
    pub fn from_toml(text: &str) -> Result<Workspace, WorkspaceError> {
        let parse = |e: toml::de::Error| WorkspaceError::Parse(e.to_string());
        let w: Workspace = match file_format(text) {
            Some(FORMAT) => toml::from_str(text).map_err(parse)?,
            Some(1) => toml::Value::Table(migrate_1(text)?)
                .try_into()
                .map_err(parse)?,
            Some(f) if f > FORMAT => return Err(WorkspaceError::NewerFormat(f)),
            Some(f) => return Err(WorkspaceError::Parse(format!("unknown format {f}"))),
            None => {
                // Not TOML, or no `format`: say which.
                toml::from_str::<toml::Table>(text).map_err(parse)?;
                return Err(WorkspaceError::Parse("missing `format`".into()));
            }
        };
        w.validate()?;
        Ok(w)
    }

    /// Serialise to TOML.
    pub fn to_toml(&self) -> Result<String, WorkspaceError> {
        toml::to_string(self).map_err(|e| WorkspaceError::Invalid(e.to_string()))
    }

    /// Check the invariants listed on [`Workspace`].
    pub fn validate(&self) -> Result<(), WorkspaceError> {
        let invalid = |m: &str| Err(WorkspaceError::Invalid(m.to_owned()));
        validate_name(&self.name)?;
        if !self.windows.first().is_some_and(Window::is_main) {
            return invalid("the first window must be the main window");
        }
        if self.windows.iter().filter(|w| w.is_main()).count() != 1 {
            return invalid("a workspace has exactly one main window");
        }
        for w in &self.windows {
            for l in w.trees() {
                if l.has_empty_stack_below_root() {
                    return invalid("only the main window's centre may be an empty tab stack");
                }
                if l.depth() > MAX_DEPTH {
                    return invalid("splits are nested too deeply");
                }
            }
            match &w.content {
                WindowContent::Floating(l) if l.is_empty() => {
                    return invalid("a floating window must hold at least one panel");
                }
                WindowContent::Main(m) => validate_main(m)?,
                _ => {}
            }
            if let Some(p) = &w.placement {
                if p.scale.is_some_and(|s| !s.is_finite() || s <= 0.0) {
                    return invalid("a display scale must be a positive number");
                }
                if p.rect
                    .is_some_and(|r| r.iter().any(|v| !v.is_finite()) || r[2] <= 0.0 || r[3] <= 0.0)
                {
                    return invalid("a window rect needs a positive width and height");
                }
            }
        }
        let mut seen: Vec<&PanelId> = Vec::new();
        for p in self.panels() {
            if seen.contains(&p) {
                return Err(WorkspaceError::Invalid(format!(
                    "panel {p} appears more than once"
                )));
            }
            seen.push(p);
        }
        Ok(())
    }

    /// The main window's centre.
    pub fn main(&self) -> &Node {
        self.main_layout().map_or(&EMPTY, |m| &m.centre)
    }

    /// The main window's centre, areas and auto-hidden panels.
    pub fn main_layout(&self) -> Option<&MainLayout> {
        self.windows.first().and_then(Window::main)
    }

    /// The main window's layout, mutably.
    pub fn main_layout_mut(&mut self) -> Option<&mut MainLayout> {
        self.windows.first_mut().and_then(Window::main_mut)
    }

    /// Every open panel: in any window's trees or auto-hidden.
    pub fn panels(&self) -> Vec<&PanelId> {
        let mut out: Vec<&PanelId> = self
            .windows
            .iter()
            .flat_map(Window::trees)
            .flat_map(Node::panels)
            .collect();
        if let Some(m) = self.main_layout() {
            out.extend(m.auto_hide.iter().flat_map(|g| g.tabs.iter()));
        }
        out
    }

    /// Panels that are the visible tab of a stack that is not collapsed, in the centre,
    /// a visible area or a floating window. Auto-hidden panels are not visible until
    /// they slide out, which is not workspace state.
    pub fn visible(&self) -> Vec<&PanelId> {
        let mut out = Vec::new();
        for w in &self.windows {
            match &w.content {
                WindowContent::Main(m) => {
                    out.extend(m.centre.visible());
                    for s in Side::ALL {
                        let a = m.area(s);
                        if let (true, Some(l)) = (a.visible, &a.layout) {
                            out.extend(l.visible());
                        }
                    }
                }
                WindowContent::Floating(l) => out.extend(l.visible()),
                WindowContent::Projector(_) => {}
            }
        }
        out
    }

    /// Window index, window stack index and tab index of panel `id`. Auto-hidden
    /// panels are in no stack: see [`MainLayout::group_of`].
    pub fn find(&self, id: &PanelId) -> Option<(usize, usize, usize)> {
        self.windows.iter().enumerate().find_map(|(w, win)| {
            let (s, t) = win.find(id)?;
            Some((w, s, t))
        })
    }

    fn group_of(&self, id: &PanelId) -> Option<usize> {
        self.main_layout().and_then(|m| m.group_of(id))
    }

    /// Is the panel open in any window, auto-hidden included?
    pub fn is_open(&self, id: &PanelId) -> bool {
        self.find(id).is_some() || self.group_of(id).is_some()
    }

    /// The window holding panel `id`, auto-hidden panels in the main window.
    fn window_of(&self, id: &PanelId) -> Option<usize> {
        self.find(id)
            .map(|(w, _, _)| w)
            .or_else(|| self.group_of(id).map(|_| 0))
    }

    /// Close a panel wherever it is. A floating window left empty is removed.
    pub fn close(&mut self, id: &PanelId) -> Result<(), WorkspaceError> {
        let w = self
            .window_of(id)
            .ok_or_else(|| WorkspaceError::NotOpen(id.clone()))?;
        self.windows[w].remove(id);
        self.drop_empty_floating();
        Ok(())
    }

    fn drop_empty_floating(&mut self) {
        self.windows.retain(|w| match &w.content {
            WindowContent::Floating(l) => !l.is_empty(),
            _ => true,
        });
    }

    /// Open a closed panel as a new tab of the main window's first stack.
    /// Opening an open panel only makes it the visible tab (of its stack, or of its
    /// edge group).
    pub fn open(&mut self, id: PanelId) -> Result<(), WorkspaceError> {
        if let Some((w, s, t)) = self.find(&id) {
            self.windows[w].activate(s, t);
            return Ok(());
        }
        let main = self.main_layout_mut().ok_or(WorkspaceError::NoTarget)?;
        if let Some(g) = main.group_of(&id) {
            let group = &mut main.auto_hide[g];
            group.active = group.tabs.iter().position(|p| *p == id).unwrap_or(0);
            return Ok(());
        }
        if main.centre.is_empty() {
            main.centre = Node::tabs(vec![id]);
        } else {
            main.centre.insert_into_stack(0, id, None);
        }
        Ok(())
    }

    /// Move a panel into window stack `stack` of window `window`.
    pub fn move_to_stack(
        &mut self,
        id: &PanelId,
        window: usize,
        stack: usize,
    ) -> Result<(), WorkspaceError> {
        let target_ok = self
            .windows
            .get(window)
            .and_then(|w| w.stack(stack))
            .is_some();
        if !target_ok {
            return Err(WorkspaceError::NoTarget);
        }
        let w = self
            .window_of(id)
            .ok_or_else(|| WorkspaceError::NotOpen(id.clone()))?;
        let found = self.windows[w].find(id);
        if found.map(|(s, _)| (w, s)) == Some((window, stack)) {
            return Ok(());
        }
        let source_stack_vanishes = self.windows[w].removal_drops_stack(id);
        let source_window_vanishes = w != 0
            && source_stack_vanishes
            && self.windows[w]
                .layout()
                .is_some_and(|l| l.panels().len() == 1);
        self.windows[w].remove(id);
        // Removing a panel prunes at most its own stack (collapsing a split keeps
        // the depth-first order of the others), so later stacks shift down by one.
        let stack = match found {
            Some((s, _)) if w == window && source_stack_vanishes && stack > s => stack - 1,
            _ => stack,
        };
        let window = if source_window_vanishes && w < window {
            window - 1
        } else {
            window
        };
        self.drop_empty_floating();
        let target = self
            .windows
            .get_mut(window)
            .ok_or(WorkspaceError::NoTarget)?;
        if target.insert_into_stack(stack, id.clone()) {
            Ok(())
        } else {
            Err(WorkspaceError::NoTarget)
        }
    }

    /// Move a panel to an edge of window `window` (the centre, in the main window).
    pub fn move_to_edge(
        &mut self,
        id: &PanelId,
        window: usize,
        edge: Edge,
    ) -> Result<(), WorkspaceError> {
        if self.windows.get(window).and_then(Window::layout).is_none() {
            return Err(WorkspaceError::NoTarget);
        }
        let w = self
            .window_of(id)
            .ok_or_else(|| WorkspaceError::NotOpen(id.clone()))?;
        let only_panel = w != 0
            && self.windows[w]
                .layout()
                .is_some_and(|l| l.panels().len() == 1);
        if only_panel && w == window {
            // The only panel of a floating window is already at every edge of it.
            return Ok(());
        }
        let source_vanishes = only_panel && w != window;
        self.windows[w].remove(id);
        let target = if source_vanishes && w < window {
            window - 1
        } else {
            window
        };
        self.drop_empty_floating();
        let window = self
            .windows
            .get_mut(target)
            .ok_or(WorkspaceError::NoTarget)?;
        // In the main window the left, right and bottom edges are the outer sides of
        // those areas (ADR-0049), which are shown; the top edge is the centre's.
        let side = match edge {
            Edge::Left => Some(Side::Left),
            Edge::Right => Some(Side::Right),
            Edge::Bottom => Some(Side::Bottom),
            Edge::Top => None,
        };
        if let (Some(m), Some(side)) = (window.main_mut(), side) {
            let area = m.area_mut(side);
            area.visible = true;
            match &mut area.layout {
                Some(l) => l.insert_at_edge(id.clone(), edge),
                None => area.layout = Some(Node::tabs(vec![id.clone()])),
            }
            return Ok(());
        }
        window
            .layout_mut()
            .ok_or(WorkspaceError::NoTarget)?
            .insert_at_edge(id.clone(), edge);
        Ok(())
    }

    /// Move a panel into a new floating window. Returns its window index.
    pub fn float(&mut self, id: &PanelId) -> Result<usize, WorkspaceError> {
        self.close(id)?;
        self.windows.push(Window {
            content: WindowContent::Floating(Node::tabs(vec![id.clone()])),
            placement: None,
        });
        Ok(self.windows.len() - 1)
    }

    /// Dock every panel of floating window `window` into the main window.
    pub fn dock(&mut self, window: usize) -> Result<(), WorkspaceError> {
        match self.windows.get(window).map(|w| &w.content) {
            Some(WindowContent::Floating(_)) => {}
            _ => return Err(WorkspaceError::NoTarget),
        }
        let w = self.windows.remove(window);
        if let WindowContent::Floating(l) = w.content {
            for id in l.panels() {
                self.open(id.clone())?;
            }
        }
        Ok(())
    }

    /// Close window `window` (not the main window): its panels close.
    pub fn close_window(&mut self, window: usize) -> Result<(), WorkspaceError> {
        if window == 0 || window >= self.windows.len() {
            return Err(WorkspaceError::NoTarget);
        }
        self.windows.remove(window);
        Ok(())
    }

    /// The workspace prepared for sharing: display fingerprints and connector
    /// names are removed, so it holds no hardware identifiers.
    pub fn exported(&self) -> Workspace {
        let mut w = self.clone();
        for win in &mut w.windows {
            if let Some(p) = &mut win.placement {
                p.display = None;
                p.connector = None;
            }
        }
        w
    }
}

/// The main window's own rules (ADR-0049 decisions 1 and 2, spec "Workspace file").
fn validate_main(m: &MainLayout) -> Result<(), WorkspaceError> {
    let invalid = |m: String| Err(WorkspaceError::Invalid(m));
    if !m.centre.has_expanded_stack() {
        return invalid("the centre keeps at least one tab stack that is not collapsed".into());
    }
    for s in Side::ALL {
        let a = m.area(s);
        if a.size > MAX_PIXELS {
            return invalid(format!("an area size is at most {MAX_PIXELS} pixels"));
        }
        if a.layout.as_ref().is_some_and(Node::is_empty) {
            return invalid("an empty area has no `layout`".into());
        }
    }
    if m.auto_hide.len() > MAX_EDGE_GROUPS {
        return invalid(format!("at most {MAX_EDGE_GROUPS} auto-hide groups"));
    }
    for g in &m.auto_hide {
        if g.tabs.is_empty() || g.tabs.len() > MAX_GROUP_TABS {
            return invalid(format!(
                "an auto-hide group holds 1 to {MAX_GROUP_TABS} panels"
            ));
        }
        if g.active >= g.tabs.len() {
            return invalid(format!(
                "active tab {} is out of range for {} tabs",
                g.active,
                g.tabs.len()
            ));
        }
        if g.size > MAX_PIXELS {
            return invalid(format!("an auto-hide size is at most {MAX_PIXELS} pixels"));
        }
        if g.anchor.as_ref().is_some_and(|a| g.tabs.contains(a)) {
            return invalid("an auto-hide group's anchor is a panel outside the group".into());
        }
    }
    Ok(())
}

static EMPTY: Node = Node::Tabs(Tabs {
    tabs: Vec::new(),
    active: 0,
    collapsed: false,
});

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::builtin;
    use crate::panel::id;

    const SPEC_EXAMPLE: &str = r#"
format = 1
name = "Minimal live"
locked = true

[[windows]]
kind = "main"
layout = { split = "vertical", ratios = [0.78, 0.22], children = [
  { tabs = ["rbs.program"] },
  { tabs = ["rbs.live-controls", "rbs.mixer"] },
] }

[[windows]]
kind = "floating"
layout = { tabs = ["rbs.stats"] }
placement = { display = "edid:DEL-41A3-7H2K9", connector = "DP-2", size = [2560, 1440], scale = 1.5, rect = [0, 0, 1280, 720] }

[[windows]]
kind = "projector"
content = "program?canvas=vertical"
placement = { display = "edid:SAM-7012-HX55", connector = "HDMI-1", size = [1920, 1080], scale = 1.0, fullscreen = true }
"#;

    #[test]
    fn reads_the_spec_example() {
        let w = Workspace::from_toml(SPEC_EXAMPLE).unwrap();
        assert!(w.locked);
        assert_eq!(w.windows.len(), 3);
        assert_eq!(w.panels().len(), 4);
        assert!(
            matches!(&w.windows[2].content, WindowContent::Projector(c) if c == "program?canvas=vertical")
        );
        let again = Workspace::from_toml(&w.to_toml().unwrap()).unwrap();
        assert_eq!(again, w);
    }

    #[test]
    fn export_strips_hardware_ids() {
        let w = Workspace::from_toml(SPEC_EXAMPLE).unwrap().exported();
        let text = w.to_toml().unwrap();
        assert!(!text.contains("edid:") && !text.contains("DP-2"), "{text}");
        assert_eq!(
            w.windows[1].placement.as_ref().unwrap().size,
            Some([2560, 1440])
        );
    }

    #[test]
    fn rejects_broken_files() {
        let cases = [
            ("format = 3\nname = \"x\"\nwindows = []", "newer"),
            ("name = \"x\"\nwindows = []", "missing"),
            ("format = 1\nname = \"x\"\nwindows = []", "first window"),
            (
                "format = 1\nname = \"x\"\n[[windows]]\nkind = \"floating\"\nlayout = { tabs = [\"rbs.mixer\"] }",
                "first window",
            ),
            (
                "format = 1\nname = \"x\"\n[[windows]]\nkind = \"main\"\nlayout = { tabs = [\"rbs.mixer\"] }\n[[windows]]\nkind = \"main\"\nlayout = { tabs = [] }",
                "exactly one",
            ),
            (
                "format = 1\nname = \"x\"\n[[windows]]\nkind = \"main\"\nlayout = { tabs = [\"rbs.mixer\", \"rbs.mixer\"] }",
                "more than once",
            ),
            (
                "format = 1\nname = \"\"\n[[windows]]\nkind = \"main\"\nlayout = { tabs = [] }",
                "name",
            ),
            (
                "format = 1\nname = \"x\"\n[[windows]]\nkind = \"projector\"\nlayout = { tabs = [] }",
                "content",
            ),
            (
                "format = 1\nname = \"x\"\nextra = 1\n[[windows]]\nkind = \"main\"\nlayout = { tabs = [] }",
                "unknown field",
            ),
        ];
        for (text, needle) in cases {
            let e = Workspace::from_toml(text).unwrap_err().to_string();
            assert!(e.contains(needle), "{needle:?} not in {e:?}");
        }
    }

    #[test]
    fn keeps_unknown_panels() {
        let text = "format = 1\nname = \"x\"\n[[windows]]\nkind = \"main\"\nlayout = { tabs = [\"com.example.chat\"] }";
        let w = Workspace::from_toml(text).unwrap();
        assert!(w.to_toml().unwrap().contains("com.example.chat"));
    }

    #[test]
    fn close_open_float_dock() {
        let mut w = builtin::edit();
        let program = id("rbs.preview");
        let n = w.panels().len();
        w.close(&program).unwrap();
        assert!(!w.is_open(&program));
        assert_eq!(
            w.close(&program),
            Err(WorkspaceError::NotOpen(program.clone()))
        );
        w.open(program.clone()).unwrap();
        assert_eq!(w.panels().len(), n);
        let f = w.float(&program).unwrap();
        assert_eq!(f, 1);
        assert_eq!(w.find(&program).map(|l| l.0), Some(1));
        w.dock(f).unwrap();
        assert_eq!(w.windows.len(), 1);
        assert!(w.is_open(&program));
        assert_eq!(w.dock(0), Err(WorkspaceError::NoTarget));
        w.validate().unwrap();
    }

    #[test]
    fn closing_last_panel_of_floating_window_removes_it() {
        let mut w = builtin::edit();
        w.float(&id("rbs.stats")).unwrap();
        assert_eq!(w.windows.len(), 2);
        w.close(&id("rbs.stats")).unwrap();
        assert_eq!(w.windows.len(), 1);
    }

    #[test]
    fn every_panel_can_be_closed_and_reopened() {
        for mut w in builtin::all() {
            let ids: Vec<PanelId> = w.panels().into_iter().cloned().collect();
            for p in &ids {
                w.close(p).unwrap();
            }
            assert!(w.main().is_empty());
            for p in &ids {
                w.open(p.clone()).unwrap();
            }
            assert_eq!(w.panels().len(), ids.len());
            w.validate().unwrap();
        }
    }

    #[test]
    fn move_between_stacks_and_windows() {
        let mut w = builtin::edit();
        let mixer = id("rbs.mixer");
        let stacks = w.main().stack_count();
        let (_, s, _) = w.find(&mixer).unwrap();
        let target = (s + 1) % stacks;
        w.move_to_stack(&mixer, 0, target).unwrap();
        w.validate().unwrap();
        assert!(w.visible().contains(&&mixer));

        // From a floating window back into the main window: the floating window goes away.
        let f = w.float(&mixer).unwrap();
        w.move_to_stack(&mixer, 0, 0).unwrap();
        assert_eq!(w.windows.len(), f);
        assert_eq!(w.find(&mixer).map(|l| (l.0, l.1)), Some((0, 0)));

        // Into a floating window's stack.
        let f = w.float(&id("rbs.stats")).unwrap();
        w.move_to_stack(&mixer, f, 0).unwrap();
        assert_eq!(w.find(&mixer).map(|l| l.0), Some(f));
        assert_eq!(w.move_to_stack(&mixer, 9, 0), Err(WorkspaceError::NoTarget));
        w.validate().unwrap();
    }

    #[test]
    fn edge_of_own_floating_window_is_a_no_op() {
        let mut w = builtin::edit();
        let stats = id("rbs.stats");
        let f = w.float(&stats).unwrap();
        w.float(&id("rbs.outputs")).unwrap();
        let before = w.clone();
        w.move_to_edge(&stats, f, Edge::Left).unwrap();
        assert_eq!(w, before);
        // Last window too: no index out of bounds.
        let g = w.float(&id("rbs.transition")).unwrap();
        w.move_to_edge(&id("rbs.transition"), g, Edge::Top).unwrap();
        w.validate().unwrap();
    }

    #[test]
    fn empty_stacks_only_at_the_main_root() {
        let base = "format = 1\nname = \"x\"\n[[windows]]\nkind = \"main\"\n";
        let nested = format!(
            "{base}layout = {{ split = \"vertical\", ratios = [0.5, 0.5], children = [{{ tabs = [] }}, {{ tabs = [\"rbs.mixer\"] }}] }}"
        );
        assert!(Workspace::from_toml(&nested).is_err());
        let floating = format!(
            "{base}layout = {{ tabs = [] }}\n[[windows]]\nkind = \"floating\"\nlayout = {{ split = \"vertical\", ratios = [0.5, 0.5], children = [{{ tabs = [] }}, {{ tabs = [] }}] }}"
        );
        assert!(Workspace::from_toml(&floating).is_err());
        assert!(Workspace::from_toml(&format!("{base}layout = {{ tabs = [] }}")).is_ok());
    }

    #[test]
    fn move_to_edge_from_floating_window() {
        let mut w = builtin::edit();
        let stats = id("rbs.stats");
        w.float(&stats).unwrap();
        w.float(&id("rbs.outputs")).unwrap();
        // Moving the panel of window 1 into window 2 renumbers window 2 to 1.
        w.move_to_edge(&stats, 2, Edge::Right).unwrap();
        assert_eq!(w.windows.len(), 2);
        assert_eq!(w.find(&stats).map(|l| l.0), Some(1));
        // The main window's left edge is the outer side of its left area.
        w.move_to_edge(&stats, 0, Edge::Left).unwrap();
        let left = main_of(&w)
            .left
            .layout
            .as_ref()
            .map(|l| l.panels().contains(&&stats));
        assert_eq!(left, Some(true));
        w.validate().unwrap();
    }

    /// The format 2 example of the spec, without the parts that come with the studio
    /// split (fixed-size children).
    const FORMAT_2: &str = r#"
format = 2
name = "Edit"
toolbar = { style = "icons-text", items = [
  "|", "ui.workspace.select", "|", "|", "scene.create", "source.add", "|",
  "~", "ui.area.toggle?area=left", "stream.toggle", "record.toggle", "|",
] }

[[windows]]
kind = "main"
centre = { tabs = ["rbs.preview"] }
left = { size = 260, layout = { split = "vertical", ratios = [0.4, 0.6], children = [
  { tabs = ["rbs.scenes"] },
  { tabs = ["rbs.sources"] },
] } }
right = { size = 290, visible = false, layout = { split = "vertical", ratios = [0.6, 0.25, 0.15], children = [
  { tabs = ["rbs.properties"] },
  { tabs = ["rbs.live-controls"] },
  { tabs = ["rbs.transition"], state = "collapsed" },
] } }
bottom = { size = 200, layout = { tabs = ["rbs.mixer"] } }
auto_hide = [
  { edge = "left", tabs = ["rbs.library"], size = 340, anchor = "rbs.scenes" },
  { edge = "right", tabs = ["rbs.stats", "rbs.outputs"], active = 1, anchor = "rbs.properties" },
]

[[windows]]
kind = "floating"
layout = { tabs = ["rbs.status-bar"] }
"#;

    fn main_of(w: &Workspace) -> &MainLayout {
        w.main_layout().unwrap()
    }

    #[test]
    fn reads_and_writes_format_2() {
        let w = Workspace::from_toml(FORMAT_2).unwrap();
        let m = main_of(&w);
        assert_eq!((m.left.size, m.right.size, m.bottom.size), (260, 290, 200));
        assert!(m.left.visible && !m.right.visible);
        assert_eq!(m.auto_hide.len(), 2);
        assert_eq!(m.auto_hide[1].size, DEFAULT_FLYOUT);
        assert_eq!(m.auto_hide[1].active, 1);
        let collapsed = w.windows[0].stack(5).unwrap();
        assert_eq!(collapsed.tabs, vec![id("rbs.transition")]);
        assert!(collapsed.collapsed);
        // Separators tidied on load.
        let items = &w.toolbar.as_ref().unwrap().items;
        assert_eq!(
            items.first(),
            Some(&ToolbarItem::Command("ui.workspace.select".into()))
        );
        assert_eq!(
            items.last(),
            Some(&ToolbarItem::Command("record.toggle".into()))
        );
        assert_eq!(
            items
                .iter()
                .filter(|i| **i == ToolbarItem::Separator)
                .count(),
            2
        );
        let again = Workspace::from_toml(&w.to_toml().unwrap()).unwrap();
        assert_eq!(again, w);
    }

    #[test]
    fn stacks_are_numbered_through_the_centre_then_the_areas() {
        let w = Workspace::from_toml(FORMAT_2).unwrap();
        let main = &w.windows[0];
        assert_eq!(main.stack_count(), 7);
        let order: Vec<&str> = (0..7)
            .map(|s| main.stack(s).unwrap().tabs[0].as_str())
            .collect();
        assert_eq!(
            order,
            [
                "rbs.preview",
                "rbs.scenes",
                "rbs.sources",
                "rbs.properties",
                "rbs.live-controls",
                "rbs.transition",
                "rbs.mixer"
            ]
        );
        assert_eq!(w.find(&id("rbs.mixer")), Some((0, 6, 0)));
        // Auto-hidden panels are open but in no stack.
        assert_eq!(w.find(&id("rbs.stats")), None);
        assert!(w.is_open(&id("rbs.stats")));
    }

    #[test]
    fn visible_skips_hidden_areas_collapsed_stacks_and_auto_hidden_panels() {
        let w = Workspace::from_toml(FORMAT_2).unwrap();
        let visible: Vec<&str> = w.visible().iter().map(|p| p.as_str()).collect();
        assert_eq!(
            visible,
            [
                "rbs.preview",
                "rbs.scenes",
                "rbs.sources",
                "rbs.mixer",
                "rbs.status-bar"
            ]
        );
    }

    #[test]
    fn format_1_becomes_the_centre() {
        let w = Workspace::from_toml(SPEC_EXAMPLE).unwrap();
        assert_eq!(w.format, FORMAT);
        let m = main_of(&w);
        assert_eq!(m.centre.panels().len(), 3);
        for s in Side::ALL {
            assert_eq!(*m.area(s), Area::empty(s));
        }
        assert!(m.auto_hide.is_empty() && w.toolbar.is_none());
        let text = w.to_toml().unwrap();
        assert!(
            text.contains("format = 2") && text.contains("centre"),
            "{text}"
        );
        assert!(
            !text.contains("left") && !text.contains("auto_hide"),
            "{text}"
        );
        // Format 2 keys are not format 1.
        let mixed = "format = 1\nname = \"x\"\n[[windows]]\nkind = \"main\"\nlayout = { tabs = [\"rbs.mixer\"], state = \"collapsed\" }";
        let e = Workspace::from_toml(mixed).unwrap_err().to_string();
        assert!(e.contains("state"), "{e}");
        let mixed =
            "format = 1\nname = \"x\"\n[[windows]]\nkind = \"main\"\ncentre = { tabs = [] }";
        assert!(Workspace::from_toml(mixed).is_err());
    }

    #[test]
    fn rejects_broken_format_2_files() {
        let main = "format = 2\nname = \"x\"\n[[windows]]\nkind = \"main\"\n";
        let cases = [
            (format!("{main}layout = {{ tabs = [] }}"), "centre"),
            (
                format!("{main}centre = {{ tabs = [\"rbs.preview\"], state = \"collapsed\" }}"),
                "not collapsed",
            ),
            (
                format!("{main}centre = {{ tabs = [] }}\nleft = {{ layout = {{ tabs = [] }} }}"),
                "empty area",
            ),
            (
                format!("{main}centre = {{ tabs = [] }}\nleft = {{ size = 20000 }}"),
                "at most",
            ),
            (
                format!(
                    "{main}centre = {{ tabs = [\"rbs.mixer\"] }}\nbottom = {{ layout = {{ tabs = [\"rbs.mixer\"] }} }}"
                ),
                "more than once",
            ),
            (
                format!(
                    "{main}centre = {{ tabs = [] }}\nauto_hide = [{{ edge = \"top\", tabs = [\"rbs.stats\"] }}]"
                ),
                "unknown variant",
            ),
            (
                format!(
                    "{main}centre = {{ tabs = [] }}\nauto_hide = [{{ edge = \"left\", tabs = [] }}]"
                ),
                "1 to 32",
            ),
            (
                format!(
                    "{main}centre = {{ tabs = [] }}\nauto_hide = [{{ edge = \"left\", tabs = [\"rbs.stats\"], active = 1 }}]"
                ),
                "out of range",
            ),
            (
                format!(
                    "{main}centre = {{ tabs = [] }}\nauto_hide = [{{ edge = \"left\", tabs = [\"rbs.stats\"], anchor = \"rbs.stats\" }}]"
                ),
                "anchor",
            ),
            (
                format!(
                    "{main}centre = {{ tabs = [] }}\n[[windows]]\nkind = \"floating\"\nlayout = {{ tabs = [\"rbs.stats\"] }}\nleft = {{ size = 200 }}"
                ),
                "only the main window",
            ),
            (
                format!(
                    "toolbar = {{ items = [\"~\", \"a.b\", \"~\"] }}\n{main}centre = {{ tabs = [] }}"
                ),
                "flexible space",
            ),
            (
                format!(
                    "toolbar = {{ items = [\"a.b\", \"a.b\"] }}\n{main}centre = {{ tabs = [] }}"
                ),
                "more than once",
            ),
            (
                format!("toolbar = {{ items = [\"Not An Id\"] }}\n{main}centre = {{ tabs = [] }}"),
                "toolbar item",
            ),
        ];
        for (text, needle) in cases {
            let e = Workspace::from_toml(&text).unwrap_err().to_string();
            assert!(e.contains(needle), "{needle:?} not in {e:?} for {text}");
        }
    }

    #[test]
    fn panels_move_out_of_areas_and_edge_groups() {
        let mut w = Workspace::from_toml(FORMAT_2).unwrap();
        // The bottom area's only panel moves into the centre: the area becomes empty.
        w.move_to_stack(&id("rbs.mixer"), 0, 0).unwrap();
        assert_eq!(main_of(&w).bottom.layout, None);
        assert_eq!(w.find(&id("rbs.mixer")), Some((0, 0, 1)));
        // A lone centre stack stays when its panel moves into an area: the area's
        // stacks keep their numbers.
        let mut v = Workspace::from_toml(FORMAT_2).unwrap();
        v.move_to_stack(&id("rbs.preview"), 0, 6).unwrap();
        assert_eq!(v.find(&id("rbs.preview")), Some((0, 6, 1)));
        assert!(v.main().is_empty());
        v.validate().unwrap();
        // A stack of an area that vanishes renumbers the later ones.
        let mut v = Workspace::from_toml(FORMAT_2).unwrap();
        v.move_to_stack(&id("rbs.scenes"), 0, 6).unwrap();
        assert_eq!(v.find(&id("rbs.scenes")), Some((0, 5, 1)));
        v.validate().unwrap();
        // Out of an edge group into a stack; the group goes when empty.
        w.move_to_stack(&id("rbs.library"), 0, 1).unwrap();
        assert_eq!(main_of(&w).auto_hide.len(), 1);
        assert!(w.find(&id("rbs.library")).is_some());
        // Opening an auto-hidden panel makes it the group's tab; closing removes it.
        w.open(id("rbs.stats")).unwrap();
        assert_eq!(main_of(&w).auto_hide[0].active, 0);
        w.close(&id("rbs.outputs")).unwrap();
        assert_eq!(main_of(&w).auto_hide[0].tabs, vec![id("rbs.stats")]);
        w.float(&id("rbs.stats")).unwrap();
        assert!(main_of(&w).auto_hide.is_empty());
        w.move_to_edge(&id("rbs.properties"), 0, Edge::Left)
            .unwrap();
        w.validate().unwrap();
    }
}
