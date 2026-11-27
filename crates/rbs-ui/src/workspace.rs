//! Workspace files: a saved arrangement of panels, floating windows and projectors.
//!
//! Spec: `docs/specs/ui-design-system.md`, "Workspace file" (format 2, ADR-0049). A
//! workspace holds layout only, never scenes, sources or output settings (glossary).
//!
//! The main window is a centre and three areas (left, right, bottom), each a tree of
//! tab stacks, plus groups of auto-hidden panels on its edges. Tab stacks of the main
//! window are numbered depth first through the centre, then the left, right and
//! bottom areas, so a stack index means the same thing to every operation.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::command::CommandId;
use crate::layout::{Axis, Edge, MAX_DEPTH, Node, RailAt, Rect, Tabs};
use crate::panel::PanelId;

mod full_height;
mod spots;

/// The workspace file format this crate writes. Format 1 files are read and
/// migrated in memory ([`Workspace::from_toml`]).
pub const FORMAT: u32 = 3;

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
    /// Left and right areas only: the area runs the full height of the window and the
    /// bottom area spans only the width beside it (ADR-0197). Ignored on the bottom.
    pub full_height: bool,
    /// Its panels; `None` when empty. Never holds an empty tab stack.
    pub layout: Option<Node>,
}

impl Area {
    /// An empty, visible area of the default size.
    pub fn empty(side: Side) -> Area {
        Area {
            size: side.default_size(),
            visible: true,
            full_height: false,
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
    #[serde(default, skip_serializing_if = "is_false")]
    full_height: bool,
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
fn is_false(b: &bool) -> bool {
    !*b
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
            full_height: self.full_height && side != Side::Bottom,
            layout: self.layout,
        }
    }

    /// `None` for an area the file does not need to mention.
    fn from_area(a: Area, side: Side) -> Option<RawArea> {
        (a != Area::empty(side)).then_some(RawArea {
            size: Some(a.size),
            visible: a.visible,
            full_height: a.full_height,
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
    type Error = String; // Text: serde's `try_from` hook makes it its message at once.

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

    /// Is `id` the Pictures leaf of this window, or one of its two pictures (ADR-0073)?
    pub fn in_pictures(&self, id: &PanelId) -> bool {
        self.main()
            .and_then(|m| m.centre.pictures_leaf())
            .is_some_and(|p| p.answers_to(id))
    }

    /// The window laid out in `content`: every tab stack shown and every divider.
    pub fn flatten(&self, content: crate::layout::Rect) -> crate::frame::WindowFlat {
        crate::frame::flatten_window(self, content, false)
    }

    /// [`Window::flatten`] with the canvas in studio mode or not (ADR-0064): a studio
    /// split shows Program and the Transition rail only in studio mode.
    pub fn flatten_in(
        &self,
        content: crate::layout::Rect,
        studio_mode: bool,
    ) -> crate::frame::WindowFlat {
        crate::frame::flatten_window(self, content, studio_mode)
    }

    /// Move divider `handle` of [`Window::flatten`] in `content` by `delta` logical
    /// pixels. Returns false when there is no such divider.
    pub fn resize(
        &mut self,
        content: crate::layout::Rect,
        handle: usize,
        delta: f32,
        studio_mode: bool,
    ) -> bool {
        use crate::frame::HandleTarget;
        let Some(h) = self
            .flatten_in(content, studio_mode)
            .handles
            .into_iter()
            .nth(handle)
        else {
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
                if m.centre.remove_pictures(id) || m.centre.remove(id) {
                    return true;
                }
                for a in [&mut m.left, &mut m.right, &mut m.bottom] {
                    if let Some(l) = &mut a.layout {
                        if l.remove(id) {
                            if l.is_empty() {
                                a.layout = None;
                                a.full_height = false;
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
    type Error = String; // Text: serde's `try_from` hook makes it its message at once.

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

const MIXER_PANEL: &str = "rbs.mixer";
const ORIENTATION_KEY: &str = "orientation";
const SCENE_SHARE_KEY: &str = "scene_share";
/// Default This scene pane share when both groups are shown (columns).
const DEFAULT_SCENE_SHARE: f64 = 0.5;
/// Inclusive bounds for a saved columns pane share.
const SCENE_SHARE_MIN: f64 = 0.05;
const SCENE_SHARE_MAX: f64 = 0.95;

/// How the Mixer arranges its sources (ADR-0083).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MixerOrientation {
    /// The panel's shape decides (ADR-0078 decision 5).
    #[default]
    Auto,
    /// Sources stacked top to bottom with horizontal meters.
    Rows,
    /// Sources side by side with vertical meters and faders.
    Columns,
}

impl MixerOrientation {
    /// The name used in the workspace file and the control API.
    pub fn name(self) -> &'static str {
        match self {
            MixerOrientation::Auto => "auto",
            MixerOrientation::Rows => "rows",
            MixerOrientation::Columns => "columns",
        }
    }

    /// The orientation named `name`; `None` for anything else.
    pub fn parse(name: &str) -> Option<MixerOrientation> {
        match name {
            "auto" => Some(MixerOrientation::Auto),
            "rows" => Some(MixerOrientation::Rows),
            "columns" => Some(MixerOrientation::Columns),
            _ => None,
        }
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
    /// Ids of the panel sections the user closed (ADR-0078), sorted. A section that
    /// is not listed is open, so an old file opens everything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed_sections: Vec<String>,
    /// View options of single panels (ADR-0083), keyed by panel id: for example
    /// `[panel_options."rbs.mixer"] orientation = "columns"`. Only non-default
    /// options are written. Unknown panels and keys are kept and ignored.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub panel_options: BTreeMap<String, BTreeMap<String, toml::Value>>,
    /// Windows; the first is the main window.
    pub windows: Vec<Window>,
    /// Where panels closed in this session were (ADR-0172); not in the file.
    #[serde(skip)]
    closed_spots: spots::ClosedSpots,
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
    /// The workspace is not built in, so it has no default layout to reset to.
    NoDefault,
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
            WorkspaceError::NoDefault => {
                f.write_str("only a built-in workspace has a default layout")
            }
        }
    }
}

impl std::error::Error for WorkspaceError {}

/// Are two workspace names the same? Names are unique ignoring case, using
/// Unicode lowercase (so "Çanlı" and "çanlı" are the same name).
pub fn same_name(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

/// Why the Pictures leaf is not collapsed, auto-hidden or floated (ADR-0073).
const PICTURES_FIXED: &str = "Preview and Program are a fixed area beside the Transition rail; \
     close it from the Panels menu to remove it";

/// The share of the Pictures leaf's place a further canvas's picture takes when it
/// opens beside it: about a 9:16 picture's width next to two 16:9 ones.
const BESIDE_PICTURES_SHARE: f32 = 0.3;

fn main_has_no_pictures(ws: &Workspace) -> bool {
    ws.main().pictures_leaf().is_none()
}

/// Pictures leaves in a tree.
fn count_pictures(n: &Node) -> usize {
    match n {
        Node::Pictures(_) => 1,
        Node::Split(s) => s.children.iter().map(count_pictures).sum(),
        Node::Tabs(_) => 0,
    }
}

/// Most closed section ids a workspace keeps.
pub const MAX_CLOSED_SECTIONS: usize = 256;
/// Longest section id.
pub const MAX_SECTION_ID_CHARS: usize = 64;

/// Check a section id (ADR-0078): 1 to 64 characters from `a-z`, `0-9`, `.`, `_`, `-`.
pub fn validate_section_id(id: &str) -> Result<(), WorkspaceError> {
    let ok = !id.is_empty()
        && id.len() <= MAX_SECTION_ID_CHARS
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        });
    if ok {
        Ok(())
    } else {
        Err(WorkspaceError::Invalid(format!(
            "a section id has 1 to {MAX_SECTION_ID_CHARS} characters from a-z, 0-9, '.', '_' and '-'"
        )))
    }
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

/// Keys format 3 added; a format 1 or 2 file using them is invalid.
const FORMAT_2_KEYS: [&str; 6] = ["centre", "left", "right", "bottom", "auto_hide", "toolbar"];

/// A format 1 file as format 2 data: the main window's `layout` becomes its `centre`
/// (areas empty and visible, nothing collapsed or auto-hidden, default toolbar).
fn migrate_1(text: &str) -> Result<toml::Table, WorkspaceError> {
    fn refuse_new_keys(v: &toml::Value) -> Result<(), WorkspaceError> {
        match v {
            toml::Value::Table(t) => {
                if let Some(k) = t.keys().find(|k| {
                    FORMAT_2_KEYS.contains(&k.as_str())
                        || k.as_str() == "state"
                        || k.as_str() == "studio"
                        || k.as_str() == "pictures"
                }) {
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
            closed_sections: Vec::new(),
            panel_options: BTreeMap::new(),
            closed_spots: spots::ClosedSpots::default(),
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
            // Format 2 (ADR-0064) differs only in the studio split, which a node reads
            // as the Pictures leaf (ADR-0073).
            Some(2) => {
                let mut t: toml::Table = toml::from_str(text).map_err(parse)?;
                t.insert("format".into(), toml::Value::Integer(i64::from(FORMAT)));
                toml::Value::Table(t).try_into().map_err(parse)?
            }
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
        if self.closed_sections.len() > MAX_CLOSED_SECTIONS {
            return invalid("too many closed sections");
        }
        for id in &self.closed_sections {
            validate_section_id(id)?;
        }
        if !self.closed_sections.windows(2).all(|w| w[0] < w[1]) {
            return invalid("closed sections are sorted and listed once");
        }
        if !self.windows.first().is_some_and(Window::is_main) {
            return invalid("the first window must be the main window");
        }
        if self.windows.iter().filter(|w| w.is_main()).count() != 1 {
            return invalid("a workspace has exactly one main window");
        }
        for (i, w) in self.windows.iter().enumerate() {
            for (t, l) in w.trees().into_iter().enumerate() {
                if count_pictures(l) > usize::from(i == 0 && t == 0) {
                    return invalid("a workspace has at most one Pictures area, in the centre");
                }
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

    /// Puts the Transition rail of the centre's Pictures leaf `at` (ADR-0073,
    /// `ui.layout.studio_rail`). [`WorkspaceError::NoTarget`] without a leaf.
    pub fn set_studio_rail(&mut self, at: RailAt) -> Result<(), WorkspaceError> {
        let m = self.main_layout_mut().ok_or(WorkspaceError::NoTarget)?;
        let p = m
            .centre
            .pictures_leaf_mut()
            .ok_or(WorkspaceError::NoTarget)?;
        p.rail = at;
        Ok(())
    }

    /// Where the centre's Pictures leaf puts the rail, if it has one.
    pub fn studio_rail(&self) -> Option<RailAt> {
        self.main().pictures_leaf().map(|p| p.rail)
    }

    /// Sets the width of the rail between the pictures, clamped to its limits
    /// (`ui.layout.rail_size`, ADR-0073). [`WorkspaceError::NoTarget`] without a leaf.
    pub fn set_rail_size(&mut self, px: u32) -> Result<(), WorkspaceError> {
        let m = self.main_layout_mut().ok_or(WorkspaceError::NoTarget)?;
        let p = m
            .centre
            .pictures_leaf_mut()
            .ok_or(WorkspaceError::NoTarget)?;
        p.rail_size = px.clamp(
            crate::layout::RAIL_SIZE_RANGE.0,
            crate::layout::RAIL_SIZE_RANGE.1,
        );
        Ok(())
    }

    /// The rail's width, if the centre has a Pictures leaf.
    pub fn rail_size(&self) -> Option<u32> {
        self.main().pictures_leaf().map(|p| p.rail_size)
    }

    /// Is `id` the Pictures leaf or one of its pictures, in the main window?
    fn pictures_answer(&self, id: &PanelId) -> bool {
        self.windows.first().is_some_and(|w| w.in_pictures(id))
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
        self.find(id).is_some() || self.group_of(id).is_some() || self.pictures_answer(id)
    }

    /// The window holding panel `id`, auto-hidden panels in the main window.
    fn window_of(&self, id: &PanelId) -> Option<usize> {
        self.find(id)
            .map(|(w, _, _)| w)
            .or_else(|| self.group_of(id).map(|_| 0))
            .or_else(|| self.pictures_answer(id).then_some(0))
    }

    /// Close a panel wherever it is, remembering where it was (ADR-0172). A floating
    /// window left empty is removed.
    pub fn close(&mut self, id: &PanelId) -> Result<(), WorkspaceError> {
        if self.is_open(id) {
            self.remember_spot(id);
        }
        self.take_out(id)
    }

    fn take_out(&mut self, id: &PanelId) -> Result<(), WorkspaceError> {
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

    /// Show the main-window area that holds window stack `stack`, when it was hidden.
    fn show_area_of(&mut self, window: usize, stack: usize) {
        if window != 0 {
            return;
        }
        let Some((tree, _)) = self.windows[window].locate(stack) else {
            return;
        };
        // Tree 0 is the centre, which is always shown.
        if tree == 0 {
            return;
        }
        let Some(m) = self.main_layout_mut() else {
            return;
        };
        let mut index = 1usize;
        for side in Side::ALL {
            if m.area(side).layout.is_some() {
                if index == tree {
                    m.area_mut(side).visible = true;
                    return;
                }
                index += 1;
            }
        }
    }

    /// Open a closed panel (ADR-0073 decisions 2 and 4). Opening an open panel makes
    /// it the visible tab, expands its stack and shows its area when that area was
    /// hidden. Opening an auto-hidden panel makes it the edge group's visible tab;
    /// the shell then slides that group out. Opening the Pictures leaf, or one of
    /// its two pictures, while the leaf exists changes nothing.
    ///
    /// A closed panel goes back where it was closed in this session when that place
    /// can still be found (ADR-0172 decision 2): into the stack of a panel it shared
    /// a stack with, beside the neighbour it shared a split with, or into its old
    /// area, edge group or floating window. Otherwise it goes into the first tab stack of the area that is its default
    /// home (Scenes and Sources left, Properties right, Mixer bottom), made visible
    /// when hidden and created when it was empty; other panels go into the centre's
    /// first tab stack, never into the Pictures leaf (it has none). `rbs.pictures`, or
    /// Preview or Program of the leaf's kind when neither is open as an ordinary
    /// panel, opens the leaf.
    pub fn open(&mut self, id: PanelId) -> Result<(), WorkspaceError> {
        if let Some((w, s, t)) = self.find(&id) {
            self.show_area_of(w, s);
            self.windows[w].activate(s, t);
            // A collapsed stack expands. The centre's last expanded stack cannot
            // collapse, so expanding never fails that rule.
            let _ = self.set_collapsed(&id, false);
            return Ok(());
        }
        if self.pictures_answer(&id) {
            return Ok(());
        }
        let main = self.main_layout_mut().ok_or(WorkspaceError::NoTarget)?;
        if let Some(g) = main.group_of(&id) {
            let group = &mut main.auto_hide[g];
            group.active = group.tabs.iter().position(|p| *p == id).unwrap_or(0);
            return Ok(());
        }
        if self.reopen_at_spot(&id) {
            return self.open(id);
        }
        let picture = |p: &PanelId| {
            matches!(p.base(), "rbs.preview" | "rbs.program") && p.param("canvas").is_none()
        };
        let wants_leaf = id.as_str() == "rbs.pictures" || picture(&id);
        if wants_leaf {
            // Only when no ordinary Preview or Program is open: the leaf would
            // hold the same panel ids twice.
            let (preview, program) = (
                crate::panel::id("rbs.preview"),
                crate::panel::id("rbs.program"),
            );
            let loose = self.is_open(&preview) || self.is_open(&program);
            if !loose && main_has_no_pictures(self) {
                return self.open_pictures();
            }
            if id.as_str() == "rbs.pictures" {
                return Ok(());
            }
        }
        // A further canvas's picture (`rbs.program?canvas=vertical`, ADR-0093 decision 7)
        // goes beside the pictures, in a stack of its own right of them (else at the
        // centre's right edge).
        if matches!(id.base(), "rbs.preview" | "rbs.program") && id.param("canvas").is_some() {
            let main = self.main_layout_mut().ok_or(WorkspaceError::NoTarget)?;
            let mut next = main.centre.clone();
            let stack = crate::layout::Tabs {
                tabs: vec![id.clone()],
                active: 0,
                collapsed: false,
            };
            if !next.insert_beside_pictures(stack, BESIDE_PICTURES_SHARE) {
                next.insert_node_at_edge(Node::tabs(vec![id.clone()]), crate::layout::Edge::Right);
            }
            if next.depth() <= MAX_DEPTH {
                main.centre = next;
                return Ok(());
            }
        }
        let side = match id.base() {
            "rbs.scenes" | "rbs.sources" => Some(Side::Left),
            "rbs.properties" => Some(Side::Right),
            "rbs.mixer" => Some(Side::Bottom),
            _ => None,
        };
        let main = self.main_layout_mut().ok_or(WorkspaceError::NoTarget)?;
        if let Some(side) = side {
            let area = main.area_mut(side);
            area.visible = true;
            match &mut area.layout {
                Some(l) if l.stack_count() > 0 => {
                    l.insert_into_stack(0, id, None);
                }
                _ => area.layout = Some(Node::tabs(vec![id])),
            }
            return Ok(());
        }
        if main.centre.is_empty() {
            main.centre = Node::tabs(vec![id]);
        } else if main.centre.stack_count() > 0 {
            main.centre.insert_into_stack(0, id, None);
        } else {
            // The centre holds only the Pictures leaf: a new stack in the right area.
            let area = main.area_mut(Side::Right);
            area.visible = true;
            match &mut area.layout {
                Some(l) if l.stack_count() > 0 => {
                    l.insert_into_stack(0, id, None);
                }
                _ => area.layout = Some(Node::tabs(vec![id])),
            }
        }
        Ok(())
    }

    /// Opens the Pictures leaf: the whole centre when it is empty, else a new first
    /// child of the centre (`conflict`-like [`WorkspaceError::Invalid`] when the tree
    /// would nest too deeply).
    fn open_pictures(&mut self) -> Result<(), WorkspaceError> {
        let main = self.main_layout_mut().ok_or(WorkspaceError::NoTarget)?;
        let mut next = main.centre.clone();
        next.insert_node_at_edge(Node::pictures(RailAt::Between), crate::layout::Edge::Left);
        if next.depth() > MAX_DEPTH {
            return Err(WorkspaceError::Invalid(
                "the centre is nested too deeply to add the Pictures area".into(),
            ));
        }
        main.centre = next;
        Ok(())
    }

    /// Move a panel into window stack `stack` of window `window`.
    pub fn move_to_stack(
        &mut self,
        id: &PanelId,
        window: usize,
        stack: usize,
    ) -> Result<(), WorkspaceError> {
        if self.pictures_answer(id) {
            return Err(WorkspaceError::Invalid(PICTURES_FIXED.into()));
        }
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
        if self.pictures_answer(id) {
            return Err(WorkspaceError::Invalid(PICTURES_FIXED.into()));
        }
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

    /// Move a panel into a new stack on the `edge` side of window stack `stack` of
    /// window `window` (a drop on that side of the five-way guide, ADR-0049 decision
    /// 5). Dropping the only tab of a stack beside that same stack changes nothing.
    pub fn move_beside(
        &mut self,
        id: &PanelId,
        window: usize,
        stack: usize,
        edge: Edge,
    ) -> Result<(), WorkspaceError> {
        if self.pictures_answer(id) {
            return Err(WorkspaceError::Invalid(PICTURES_FIXED.into()));
        }
        let target = self
            .windows
            .get(window)
            .and_then(|w| w.stack(stack))
            .ok_or(WorkspaceError::NoTarget)?;
        // The target is found again by one of its other panels once `id` is out.
        let Some(anchor) = target.tabs.iter().find(|p| *p != id).cloned() else {
            return Ok(());
        };
        let w = self
            .window_of(id)
            .ok_or_else(|| WorkspaceError::NotOpen(id.clone()))?;
        self.windows[w].remove(id);
        self.drop_empty_floating();
        let (w, stack, _) = self.find(&anchor).ok_or(WorkspaceError::NoTarget)?;
        let win = &mut self.windows[w];
        let (tree, local) = win.locate(stack).ok_or(WorkspaceError::NoTarget)?;
        let new = Tabs {
            tabs: vec![id.clone()],
            active: 0,
            collapsed: false,
        };
        let done = win
            .trees_mut()
            .into_iter()
            .nth(tree)
            .is_some_and(|t| t.insert_beside(local, new, edge));
        if done {
            Ok(())
        } else {
            Err(WorkspaceError::NoTarget)
        }
    }

    /// Collapse (`ui.panel.collapse`) or expand (`ui.panel.expand`) the stack holding
    /// panel `id`, in any window. The centre keeps one stack that is not collapsed.
    pub fn set_collapsed(&mut self, id: &PanelId, collapsed: bool) -> Result<(), WorkspaceError> {
        if self.pictures_answer(id) {
            return Err(WorkspaceError::Invalid(PICTURES_FIXED.into()));
        }
        let (w, stack, _) = self
            .find(id)
            .ok_or_else(|| WorkspaceError::NotOpen(id.clone()))?;
        let win = &mut self.windows[w];
        let (tree, local) = win.locate(stack).ok_or(WorkspaceError::NoTarget)?;
        let t = win
            .trees_mut()
            .into_iter()
            .nth(tree)
            .and_then(|t| t.stack_mut(local))
            .ok_or(WorkspaceError::NoTarget)?;
        t.collapsed = collapsed;
        if collapsed && tree == 0 && win.is_main() && !win.trees()[0].has_expanded_stack() {
            t_restore(win, tree, local);
            return Err(WorkspaceError::Invalid(
                "the centre keeps at least one tab stack that is not collapsed".into(),
            ));
        }
        Ok(())
    }

    /// Auto-hide the stack holding panel `id` (`ui.panel.auto_hide`): it leaves the
    /// main window's tree and becomes edge tabs on its area's edge, or for the centre
    /// on the nearest of the left, right and bottom edges (ADR-0049 decision 2).
    /// Floating windows have no edges to hide to, and the centre keeps a panel.
    pub fn auto_hide(&mut self, id: &PanelId, content: Rect) -> Result<(), WorkspaceError> {
        if self.pictures_answer(id) {
            return Err(WorkspaceError::Invalid(PICTURES_FIXED.into()));
        }
        let (w, stack, _) = self
            .find(id)
            .ok_or_else(|| WorkspaceError::NotOpen(id.clone()))?;
        if w != 0 {
            return Err(WorkspaceError::NoTarget);
        }
        let edge = {
            let win = &self.windows[0];
            let (tree, local) = win.locate(stack).ok_or(WorkspaceError::NoTarget)?;
            let m = win.main().ok_or(WorkspaceError::NoTarget)?;
            let sides: Vec<Side> = Side::ALL
                .into_iter()
                .filter(|s| m.area(*s).layout.is_some())
                .collect();
            match tree.checked_sub(1).and_then(|t| sides.get(t)) {
                Some(side) => *side,
                None => {
                    if matches!(m.centre, Node::Tabs(_)) {
                        return Err(WorkspaceError::NoTarget);
                    }
                    let centre = crate::frame::main_rects(m, content).centre;
                    let r = m
                        .centre
                        .flatten(centre)
                        .stacks
                        .iter()
                        .find(|s| s.stack == local)
                        .map_or(centre, |s| s.rect);
                    nearest_edge(r, centre)
                }
            }
        };
        let m = self.main_layout_mut().ok_or(WorkspaceError::NoTarget)?;
        let (tabs, anchor) = take_from_main(m, id).ok_or(WorkspaceError::NoTarget)?;
        m.auto_hide.push(EdgeGroup {
            edge,
            active: tabs.active.min(tabs.tabs.len().saturating_sub(1)),
            tabs: tabs.tabs,
            size: DEFAULT_FLYOUT,
            anchor,
        });
        Ok(())
    }

    /// Pin auto-hidden panel `id` (`ui.panel.pin`): its whole group docks back next to
    /// its anchor when that panel is docked in the main window, else at the outer side
    /// of the area of its edge, which is shown.
    pub fn pin(&mut self, id: &PanelId) -> Result<(), WorkspaceError> {
        let m = self.main_layout_mut().ok_or(WorkspaceError::NoTarget)?;
        let g = m
            .group_of(id)
            .ok_or_else(|| WorkspaceError::NotOpen(id.clone()))?;
        let group = m.auto_hide.remove(g);
        let axis = match group.edge {
            Side::Left | Side::Right => Axis::Vertical,
            Side::Bottom => Axis::Horizontal,
        };
        let stack = Tabs {
            active: group.active,
            tabs: group.tabs,
            collapsed: false,
        };
        if let Some(anchor) = &group.anchor {
            let docked = std::iter::once(&mut m.centre).chain(
                [&mut m.left, &mut m.right, &mut m.bottom]
                    .into_iter()
                    .filter_map(|a| a.layout.as_mut()),
            );
            for tree in docked {
                if tree.find(anchor).is_some() {
                    tree.insert_stack_beside(anchor, stack, axis);
                    return Ok(());
                }
            }
        }
        let side = group.edge;
        let edge = match side {
            Side::Left => Edge::Left,
            Side::Right => Edge::Right,
            Side::Bottom => Edge::Bottom,
        };
        let area = m.area_mut(side);
        area.visible = true;
        let node = Node::Tabs(stack);
        match &mut area.layout {
            Some(l) => l.insert_node_at_edge(node, edge),
            None => area.layout = Some(node),
        }
        Ok(())
    }

    /// Move a panel into a new floating window. Returns its window index.
    pub fn float(&mut self, id: &PanelId) -> Result<usize, WorkspaceError> {
        if self.pictures_answer(id) {
            return Err(WorkspaceError::Invalid(PICTURES_FIXED.into()));
        }
        self.take_out(id)?;
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
        let panels: Vec<PanelId> = self.windows[window]
            .layout()
            .map(|l| l.panels().into_iter().cloned().collect())
            .unwrap_or_default();
        panels.iter().for_each(|p| self.remember_spot(p));
        self.windows.remove(window);
        Ok(())
    }

    /// The projectors this build opens (ADR-0125): each one's window index and parsed
    /// content, in window order. Projectors of other content stay in the file unopened.
    pub fn projectors(&self) -> Vec<(usize, crate::projector::ProjectorContent)> {
        self.windows
            .iter()
            .enumerate()
            .filter_map(|(i, w)| match &w.content {
                WindowContent::Projector(c) => {
                    crate::projector::ProjectorContent::parse(c).map(|p| (i, p))
                }
                _ => None,
            })
            .collect()
    }

    /// Opens the projector of `content`'s canvas, or replaces its content and placement
    /// when one of that canvas is open (one projector per canvas, ADR-0125 decision 9).
    /// A projector is not a layout step: it is saved with the workspace, not undone.
    pub fn set_projector(
        &mut self,
        content: &crate::projector::ProjectorContent,
        placement: Option<Placement>,
    ) {
        let text = content.to_string();
        match self
            .projectors()
            .into_iter()
            .find(|(_, p)| p.key() == content.key())
        {
            Some((i, _)) => {
                self.windows[i].content = WindowContent::Projector(text);
                self.windows[i].placement = placement;
            }
            None => self.windows.push(Window {
                content: WindowContent::Projector(text),
                placement,
            }),
        }
    }

    /// Closes the projector of the canvas `key` names ([`ProjectorContent::key`]);
    /// `false` when none is open.
    ///
    /// [`ProjectorContent::key`]: crate::projector::ProjectorContent::key
    pub fn close_projector(&mut self, key: &str) -> bool {
        let Some((i, _)) = self.projectors().into_iter().find(|(_, p)| p.key() == key) else {
            return false;
        };
        self.windows.remove(i);
        true
    }

    /// Is the section `id` closed (ADR-0078)?
    pub fn section_closed(&self, id: &str) -> bool {
        self.closed_sections
            .binary_search_by(|s| s.as_str().cmp(id))
            .is_ok()
    }

    /// Close or open the section `id`. Opening an id that is not listed is a no-op.
    pub fn set_section_closed(&mut self, id: &str, closed: bool) -> Result<(), WorkspaceError> {
        validate_section_id(id)?;
        let found = self
            .closed_sections
            .binary_search_by(|s| s.as_str().cmp(id));
        match (found, closed) {
            (Err(at), true) => {
                if self.closed_sections.len() >= MAX_CLOSED_SECTIONS {
                    return Err(WorkspaceError::Invalid(format!(
                        "at most {MAX_CLOSED_SECTIONS} closed sections are kept"
                    )));
                }
                self.closed_sections.insert(at, id.to_owned());
            }
            (Ok(at), false) => {
                self.closed_sections.remove(at);
            }
            _ => {}
        }
        Ok(())
    }

    /// The Mixer's orientation (ADR-0083). A missing or invalid value is `Auto`.
    pub fn mixer_orientation(&self) -> MixerOrientation {
        self.panel_options
            .get(MIXER_PANEL)
            .and_then(|o| o.get(ORIENTATION_KEY))
            .and_then(toml::Value::as_str)
            .and_then(MixerOrientation::parse)
            .unwrap_or_default()
    }

    /// Set the Mixer's orientation. `Auto` removes the key, so the file stays as an
    /// older build writes it.
    pub fn set_mixer_orientation(&mut self, orientation: MixerOrientation) {
        if orientation == MixerOrientation::Auto {
            if let Some(options) = self.panel_options.get_mut(MIXER_PANEL) {
                options.remove(ORIENTATION_KEY);
                if options.is_empty() {
                    self.panel_options.remove(MIXER_PANEL);
                }
            }
        } else {
            self.panel_options
                .entry(MIXER_PANEL.to_owned())
                .or_default()
                .insert(
                    ORIENTATION_KEY.to_owned(),
                    toml::Value::String(orientation.name().to_owned()),
                );
        }
    }

    /// This scene pane share of the Mixer columns groups row (ADR-0101). Missing or
    /// invalid means half; out-of-range values are clamped.
    pub fn mixer_scene_share(&self) -> f32 {
        let raw = self
            .panel_options
            .get(MIXER_PANEL)
            .and_then(|o| o.get(SCENE_SHARE_KEY))
            .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
            .unwrap_or(DEFAULT_SCENE_SHARE);
        raw.clamp(SCENE_SHARE_MIN, SCENE_SHARE_MAX) as f32
    }

    /// Set the Mixer columns This scene pane share. `0.5` removes the key.
    pub fn set_mixer_scene_share(&mut self, share: f32) {
        let clamped = (share as f64).clamp(SCENE_SHARE_MIN, SCENE_SHARE_MAX);
        if (clamped - DEFAULT_SCENE_SHARE).abs() < f64::EPSILON {
            if let Some(options) = self.panel_options.get_mut(MIXER_PANEL) {
                options.remove(SCENE_SHARE_KEY);
                if options.is_empty() {
                    self.panel_options.remove(MIXER_PANEL);
                }
            }
            return;
        }
        self.panel_options
            .entry(MIXER_PANEL.to_owned())
            .or_default()
            .insert(SCENE_SHARE_KEY.to_owned(), toml::Value::Float(clamped));
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
/// Undo a collapse that broke the centre rule.
fn t_restore(win: &mut Window, tree: usize, local: usize) {
    if let Some(t) = win
        .trees_mut()
        .into_iter()
        .nth(tree)
        .and_then(|t| t.stack_mut(local))
    {
        t.collapsed = false;
    }
}

/// Take the stack holding `id` out of the centre or an area; an area left without
/// panels becomes empty.
fn take_from_main(m: &mut MainLayout, id: &PanelId) -> Option<(Tabs, Option<PanelId>)> {
    if m.centre.find(id).is_some() {
        return m.centre.take_stack_of(id);
    }
    for a in [&mut m.left, &mut m.right, &mut m.bottom] {
        if let Some(l) = &mut a.layout {
            if l.find(id).is_some() {
                let out = l.take_stack_of(id);
                if l.is_empty() {
                    a.layout = None;
                    a.full_height = false;
                }
                return out;
            }
        }
    }
    None
}

/// The edge of `outer` nearest to `r` among left, right and bottom; ties go in that
/// order (ADR-0049 decision 2: there are no top edge tabs).
fn nearest_edge(r: Rect, outer: Rect) -> Side {
    let left = r.x - outer.x;
    let right = (outer.x + outer.w) - (r.x + r.w);
    let bottom = (outer.y + outer.h) - (r.y + r.h);
    if left <= right && left <= bottom {
        Side::Left
    } else if right <= bottom {
        Side::Right
    } else {
        Side::Bottom
    }
}

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
mod tests;
