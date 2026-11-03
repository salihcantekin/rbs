//! Workspace files: a saved arrangement of panels, floating windows and projectors.
//!
//! Spec: `docs/specs/ui-design-system.md`, "Workspace file". A workspace holds
//! layout only, never scenes, sources or output settings (glossary).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::layout::{Edge, MAX_DEPTH, Node};
use crate::panel::PanelId;

/// The workspace file format this crate writes.
pub const FORMAT: u32 = 1;

/// Longest workspace name in characters.
pub const MAX_NAME_CHARS: usize = 64;

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

/// What a window shows.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowContent {
    /// The main window, with its layout. Exactly one per workspace, listed first.
    Main(Node),
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
    /// The layout tree, for main and floating windows.
    pub fn layout(&self) -> Option<&Node> {
        match &self.content {
            WindowContent::Main(n) | WindowContent::Floating(n) => Some(n),
            WindowContent::Projector(_) => None,
        }
    }

    /// The layout tree, mutably.
    pub fn layout_mut(&mut self) -> Option<&mut Node> {
        match &mut self.content {
            WindowContent::Main(n) | WindowContent::Floating(n) => Some(n),
            WindowContent::Projector(_) => None,
        }
    }

    /// Is this the main window?
    pub fn is_main(&self) -> bool {
        matches!(self.content, WindowContent::Main(_))
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
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    placement: Option<Placement>,
}

impl TryFrom<RawWindow> for Window {
    type Error = String;

    fn try_from(r: RawWindow) -> Result<Self, String> {
        let content = match (r.kind, r.layout, r.content) {
            (RawKind::Main, Some(l), None) => WindowContent::Main(l),
            (RawKind::Floating, Some(l), None) if !l.is_empty() => WindowContent::Floating(l),
            (RawKind::Floating, Some(_), None) => {
                return Err("a floating window must hold at least one panel".into());
            }
            (RawKind::Projector, None, Some(c)) if !c.trim().is_empty() => {
                WindowContent::Projector(c)
            }
            (RawKind::Main | RawKind::Floating, _, _) => {
                return Err("main and floating windows have a `layout` and no `content`".into());
            }
            (RawKind::Projector, _, _) => {
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
        let (kind, layout, content) = match w.content {
            WindowContent::Main(l) => (RawKind::Main, Some(l), None),
            WindowContent::Floating(l) => (RawKind::Floating, Some(l), None),
            WindowContent::Projector(c) => (RawKind::Projector, None, Some(c)),
        };
        RawWindow {
            kind,
            layout,
            content,
            placement: w.placement,
        }
    }
}

/// A workspace.
///
/// Invariants, checked by [`Workspace::validate`] (called by [`Workspace::from_toml`]):
/// exactly one main window and it is first; a panel id appears at most once in
/// the whole workspace; only the main window's root may be an empty stack;
/// splits are at most [`MAX_DEPTH`] deep. The operations below keep all of them
/// except the depth limit, which repeated edge moves can exceed;
/// [`crate::LayoutSession::apply`] validates every result and refuses such a change.
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

impl Workspace {
    /// A workspace with one main window holding `layout`.
    pub fn new(name: &str, layout: Node) -> Workspace {
        Workspace {
            format: FORMAT,
            name: name.to_owned(),
            locked: false,
            windows: vec![Window {
                content: WindowContent::Main(layout),
                placement: None,
            }],
        }
    }

    /// Parse and validate a workspace file. Older formats are migrated here
    /// when they exist; format 1 is the first.
    pub fn from_toml(text: &str) -> Result<Workspace, WorkspaceError> {
        #[derive(Deserialize)]
        struct Version {
            format: Option<u32>,
        }
        let v: Version = toml::from_str(text).map_err(|e| WorkspaceError::Parse(e.to_string()))?;
        match v.format {
            Some(FORMAT) => {}
            Some(f) if f > FORMAT => return Err(WorkspaceError::NewerFormat(f)),
            Some(f) => {
                return Err(WorkspaceError::Parse(format!("unknown format {f}")));
            }
            None => return Err(WorkspaceError::Parse("missing `format`".into())),
        }
        let w: Workspace =
            toml::from_str(text).map_err(|e| WorkspaceError::Parse(e.to_string()))?;
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
        let mut seen: Vec<&PanelId> = Vec::new();
        for w in &self.windows {
            if let Some(l) = w.layout() {
                if l.has_empty_stack_below_root() {
                    return invalid("only the main window's root may be an empty tab stack");
                }
                if matches!(w.content, WindowContent::Floating(_)) && l.is_empty() {
                    return invalid("a floating window must hold at least one panel");
                }
                if l.depth() > MAX_DEPTH {
                    return invalid("splits are nested too deeply");
                }
                for p in l.panels() {
                    if seen.contains(&p) {
                        return Err(WorkspaceError::Invalid(format!(
                            "panel {p} appears more than once"
                        )));
                    }
                    seen.push(p);
                }
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
        Ok(())
    }

    /// The main window's layout.
    pub fn main(&self) -> &Node {
        match self.windows.first().map(|w| &w.content) {
            Some(WindowContent::Main(n)) => n,
            // Unreachable for a validated workspace; an empty layout is a safe answer.
            _ => &EMPTY,
        }
    }

    fn main_mut(&mut self) -> Option<&mut Node> {
        match self.windows.first_mut().map(|w| &mut w.content) {
            Some(WindowContent::Main(n)) => Some(n),
            _ => None,
        }
    }

    /// Every open panel.
    pub fn panels(&self) -> Vec<&PanelId> {
        self.windows
            .iter()
            .filter_map(Window::layout)
            .flat_map(Node::panels)
            .collect()
    }

    /// Panels that are the visible tab of a stack in any window.
    pub fn visible(&self) -> Vec<&PanelId> {
        self.windows
            .iter()
            .filter_map(Window::layout)
            .flat_map(Node::visible)
            .collect()
    }

    /// Window index, stack index and tab index of panel `id`.
    pub fn find(&self, id: &PanelId) -> Option<(usize, usize, usize)> {
        self.windows.iter().enumerate().find_map(|(w, win)| {
            let (s, t) = win.layout()?.find(id)?;
            Some((w, s, t))
        })
    }

    /// Is the panel open in any window?
    pub fn is_open(&self, id: &PanelId) -> bool {
        self.find(id).is_some()
    }

    /// Close a panel wherever it is. A floating window left empty is removed.
    pub fn close(&mut self, id: &PanelId) -> Result<(), WorkspaceError> {
        let (w, _, _) = self
            .find(id)
            .ok_or_else(|| WorkspaceError::NotOpen(id.clone()))?;
        if let Some(l) = self.windows[w].layout_mut() {
            l.remove(id);
        }
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
    /// Opening an open panel only makes it the visible tab.
    pub fn open(&mut self, id: PanelId) -> Result<(), WorkspaceError> {
        if let Some((w, s, t)) = self.find(&id) {
            if let Some(l) = self.windows[w].layout_mut() {
                l.activate(s, t);
            }
            return Ok(());
        }
        let main = self.main_mut().ok_or(WorkspaceError::NoTarget)?;
        if main.is_empty() {
            *main = Node::tabs(vec![id]);
        } else {
            main.insert_into_stack(0, id, None);
        }
        Ok(())
    }

    /// Move a panel into stack `stack` of window `window`.
    pub fn move_to_stack(
        &mut self,
        id: &PanelId,
        window: usize,
        stack: usize,
    ) -> Result<(), WorkspaceError> {
        let target_ok = self
            .windows
            .get(window)
            .and_then(Window::layout)
            .and_then(|l| l.stack(stack))
            .is_some();
        if !target_ok {
            return Err(WorkspaceError::NoTarget);
        }
        let (w, s, _) = self
            .find(id)
            .ok_or_else(|| WorkspaceError::NotOpen(id.clone()))?;
        if (w, s) == (window, stack) {
            return Ok(());
        }
        let source_stack_vanishes = self.windows[w]
            .layout()
            .and_then(|l| l.stack(s))
            .is_some_and(|t| t.tabs.len() == 1);
        let source_window_vanishes = w != 0 && source_stack_vanishes && {
            let l = self.windows[w].layout();
            l.is_some_and(|l| l.panels().len() == 1)
        };
        if let Some(l) = self.windows[w].layout_mut() {
            l.remove(id);
        }
        // Removing a panel prunes at most its own stack (collapsing a split keeps
        // the depth-first order of the others), so later stacks shift down by one.
        let stack = if w == window && source_stack_vanishes && stack > s {
            stack - 1
        } else {
            stack
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
            .and_then(Window::layout_mut)
            .ok_or(WorkspaceError::NoTarget)?;
        if target.insert_into_stack(stack, id.clone(), None) {
            Ok(())
        } else {
            Err(WorkspaceError::NoTarget)
        }
    }

    /// Move a panel to an edge of window `window`.
    pub fn move_to_edge(
        &mut self,
        id: &PanelId,
        window: usize,
        edge: Edge,
    ) -> Result<(), WorkspaceError> {
        if self.windows.get(window).and_then(Window::layout).is_none() {
            return Err(WorkspaceError::NoTarget);
        }
        let (w, _, _) = self
            .find(id)
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
        if let Some(l) = self.windows[w].layout_mut() {
            l.remove(id);
        }
        let target = if source_vanishes && w < window {
            window - 1
        } else {
            window
        };
        self.drop_empty_floating();
        let l = self
            .windows
            .get_mut(target)
            .and_then(Window::layout_mut)
            .ok_or(WorkspaceError::NoTarget)?;
        l.insert_at_edge(id.clone(), edge);
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

static EMPTY: Node = Node::Tabs(crate::layout::Tabs {
    tabs: Vec::new(),
    active: 0,
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
            ("format = 2\nname = \"x\"\nwindows = []", "newer"),
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
        w.move_to_edge(&stats, 0, Edge::Left).unwrap();
        assert_eq!(w.find(&stats), Some((0, 0, 0)));
        w.validate().unwrap();
    }
}
