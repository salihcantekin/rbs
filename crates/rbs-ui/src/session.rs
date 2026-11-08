//! The layout state of one UI client: the current workspace, its layout undo
//! history and the live status that the layout rules depend on.
//!
//! Every change goes through [`LayoutCommand`], which mirrors the client-scoped
//! `ui.*` control API commands (`docs/specs/control-api.md`, UI rule U20), so a
//! shortcut, the command palette, a pointer and a Stream Deck all take the same path.

use crate::history::LayoutHistory;
use crate::layout::{Edge, Rect};
use crate::live::LiveStatus;
use crate::panel::{PanelId, UpdateRate, built_in, needed_rate};
use crate::workspace::{Side, Workspace, WorkspaceError};

/// A change to the layout.
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutCommand {
    /// Open a panel, or show it if it is open (`ui.panel.open`).
    OpenPanel(PanelId),
    /// Close a panel (`ui.panel.close`).
    ClosePanel(PanelId),
    /// Show tab `tab` of stack `stack` in window `window`.
    ActivateTab {
        /// Window index.
        window: usize,
        /// Stack index.
        stack: usize,
        /// Tab index.
        tab: usize,
    },
    /// Move a panel into a tab stack.
    MoveToStack {
        /// Panel.
        panel: PanelId,
        /// Target window.
        window: usize,
        /// Target stack.
        stack: usize,
    },
    /// Move a panel to an edge of a window.
    MoveToEdge {
        /// Panel.
        panel: PanelId,
        /// Target window.
        window: usize,
        /// Edge.
        edge: Edge,
    },
    /// Move a panel into a new stack beside a stack: a drop on one side of the
    /// five-way guide (ADR-0049 decision 5).
    MoveBeside {
        /// Panel.
        panel: PanelId,
        /// Target window.
        window: usize,
        /// Target stack.
        stack: usize,
        /// The side of the stack the panel goes to.
        edge: Edge,
    },
    /// Float a panel into its own window.
    Float(PanelId),
    /// Dock a floating window's panels into the main window.
    Dock {
        /// Floating window index.
        window: usize,
    },
    /// Close a floating window or projector, closing its panels.
    CloseWindow {
        /// Window index.
        window: usize,
    },
    /// Move a splitter of window `window`, laid out in `area`, by `delta` logical
    /// pixels. Moves with the same `group` join one undo step.
    Resize {
        /// Window index.
        window: usize,
        /// Area the window's layout was flattened into.
        area: Rect,
        /// Handle index in the flattened layout.
        handle: usize,
        /// Distance in logical pixels.
        delta: f32,
        /// Undo group, for example one per pointer drag.
        group: Option<u64>,
        /// The window was laid out with the canvas in studio mode (ADR-0064).
        studio_mode: bool,
    },
    /// Put the centre's Transition rail between or below the pictures
    /// (`ui.layout.studio_rail`, ADR-0064).
    SetStudioRail(crate::layout::RailAt),
    /// Lock or unlock the layout against pointer changes.
    SetLocked(bool),
    /// Replace the layout with another workspace (`ui.workspace.switch`).
    Switch(Workspace),
    /// Show or hide an area of the main window (`ui.area.show`, `ui.area.hide`). A
    /// hidden area keeps its panels and size.
    SetAreaVisible {
        /// The area.
        side: Side,
        /// Shown.
        visible: bool,
    },
    /// Show (`ui.area.show_all`) or hide (`ui.area.hide_all`) all three areas.
    SetAllAreasVisible(bool),
    /// Give every area its default size (`ui.layout.reset_sizes`).
    ResetSizes,
    /// Put the workspace's default toolbar back (`ui.toolbar.reset`).
    ResetToolbar,
    /// Replace a built-in workspace's layout, toolbar and lock with its default
    /// (`ui.workspace.reset`). One undo step; [`WorkspaceError::NoDefault`] for a
    /// workspace that is not built in. A copy saved by an older RBS gets what the
    /// default has gained since, such as the studio split (ADR-0064).
    ResetWorkspace,
    /// Collapse (`ui.panel.collapse`) or expand (`ui.panel.expand`) the stack holding
    /// a panel. Allowed while locked: it hides without rearranging.
    SetCollapsed {
        /// Panel.
        panel: PanelId,
        /// Collapsed.
        collapsed: bool,
    },
    /// Auto-hide the stack holding a panel to an edge of the main window
    /// (`ui.panel.auto_hide`).
    AutoHide {
        /// Panel.
        panel: PanelId,
        /// The main window's content rectangle, which decides the nearest edge for a
        /// stack in the centre.
        content: Rect,
    },
    /// Dock an auto-hidden panel's group back (`ui.panel.pin`).
    Pin(PanelId),
}

/// Who asked for a change. A locked workspace refuses pointer moves, resizes
/// and close buttons; commands from the palette, shortcuts and the control API
/// still apply (spec, "Workspace file").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Direct manipulation with a mouse, pen or touch.
    Pointer,
    /// Keyboard shortcut, command palette, menu or control API.
    Command,
}

/// A notice the shell shows after a change.
#[derive(Clone, Debug, PartialEq)]
pub enum Toast {
    /// A guarded panel was closed while live, directly, with its window, or by
    /// a workspace switch: "Program closed. Undo" (rule U21).
    ClosedWhileLive(PanelId),
    /// The workspace was switched automatically on going live or ending the
    /// stream; the toast offers a one-click way back to `previous` (rule U21).
    SwitchedAutomatically {
        /// Name of the workspace shown before.
        previous: String,
    },
}

/// Why a command was refused.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandError {
    /// The layout is locked against pointer changes.
    Locked,
    /// The command does not apply to the current layout.
    Workspace(WorkspaceError),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::Locked => f.write_str("the layout is locked"),
            CommandError::Workspace(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for CommandError {}

impl From<WorkspaceError> for CommandError {
    fn from(e: WorkspaceError) -> Self {
        CommandError::Workspace(e)
    }
}

/// Layout state of one UI client.
#[derive(Clone, Debug)]
pub struct LayoutSession {
    workspace: Workspace,
    history: LayoutHistory,
    live: LiveStatus,
}

impl LayoutSession {
    /// A session showing `workspace`.
    pub fn new(workspace: Workspace) -> Self {
        LayoutSession {
            workspace,
            history: LayoutHistory::default(),
            live: LiveStatus::default(),
        }
    }

    /// The current workspace.
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// The live status last reported by the engine.
    pub fn live(&self) -> &LiveStatus {
        &self.live
    }

    /// Update the live status.
    pub fn set_live(&mut self, live: LiveStatus) {
        self.live = live;
    }

    /// Apply a command. On success the previous layout is in the undo history.
    pub fn apply(
        &mut self,
        command: LayoutCommand,
        origin: Origin,
    ) -> Result<Option<Toast>, CommandError> {
        let pointer_blocked = matches!(
            command,
            LayoutCommand::ClosePanel(_)
                | LayoutCommand::MoveToStack { .. }
                | LayoutCommand::MoveToEdge { .. }
                | LayoutCommand::MoveBeside { .. }
                | LayoutCommand::Float(_)
                | LayoutCommand::Dock { .. }
                | LayoutCommand::CloseWindow { .. }
                | LayoutCommand::Resize { .. }
                | LayoutCommand::ResetToolbar
                | LayoutCommand::AutoHide { .. }
                | LayoutCommand::Pin(_)
        );
        if origin == Origin::Pointer && self.workspace.locked && pointer_blocked {
            return Err(CommandError::Locked);
        }
        let before = self.workspace.clone();
        let mut next = self.workspace.clone();
        let mut group = None;
        let mut closed: Vec<PanelId> = Vec::new();
        match command {
            LayoutCommand::OpenPanel(id) => next.open(id)?,
            LayoutCommand::ClosePanel(id) => {
                next.close(&id)?;
                closed.push(id);
            }
            LayoutCommand::ActivateTab { window, stack, tab } => {
                let ok = next
                    .windows
                    .get_mut(window)
                    .is_some_and(|w| w.activate(stack, tab));
                if !ok {
                    return Err(WorkspaceError::NoTarget.into());
                }
            }
            LayoutCommand::MoveToStack {
                panel,
                window,
                stack,
            } => next.move_to_stack(&panel, window, stack)?,
            LayoutCommand::MoveToEdge {
                panel,
                window,
                edge,
            } => next.move_to_edge(&panel, window, edge)?,
            LayoutCommand::MoveBeside {
                panel,
                window,
                stack,
                edge,
            } => next.move_beside(&panel, window, stack, edge)?,
            LayoutCommand::Float(id) => {
                next.float(&id)?;
            }
            LayoutCommand::Dock { window } => next.dock(window)?,
            LayoutCommand::CloseWindow { window } => {
                if let Some(l) = next.windows.get(window).and_then(|w| w.layout()) {
                    closed.extend(l.panels().into_iter().cloned());
                }
                next.close_window(window)?;
            }
            LayoutCommand::Resize {
                window,
                area,
                handle,
                delta,
                group: g,
                studio_mode,
            } => {
                let ok = next
                    .windows
                    .get_mut(window)
                    .is_some_and(|w| w.resize(area, handle, delta, studio_mode));
                if !ok {
                    return Err(WorkspaceError::NoTarget.into());
                }
                group = g;
            }
            LayoutCommand::SetLocked(locked) => next.locked = locked,
            LayoutCommand::SetAreaVisible { side, visible } => {
                next.main_layout_mut()
                    .ok_or(WorkspaceError::NoTarget)?
                    .area_mut(side)
                    .visible = visible;
            }
            LayoutCommand::SetAllAreasVisible(visible) => {
                let m = next.main_layout_mut().ok_or(WorkspaceError::NoTarget)?;
                for side in Side::ALL {
                    m.area_mut(side).visible = visible;
                }
            }
            LayoutCommand::ResetSizes => {
                let m = next.main_layout_mut().ok_or(WorkspaceError::NoTarget)?;
                for side in Side::ALL {
                    m.area_mut(side).size = side.default_size();
                }
            }
            LayoutCommand::ResetToolbar => next.toolbar = None,
            LayoutCommand::ResetWorkspace => {
                let mut default =
                    crate::builtin::by_name(&before.name).ok_or(WorkspaceError::NoDefault)?;
                // The name keeps the user's spelling, which names the saved file.
                default.name = before.name.clone();
                closed.extend(
                    before
                        .panels()
                        .into_iter()
                        .filter(|p| !default.is_open(p))
                        .cloned(),
                );
                next = default;
            }
            LayoutCommand::SetStudioRail(at) => next.set_studio_rail(at)?,
            LayoutCommand::SetCollapsed { panel, collapsed } => {
                next.set_collapsed(&panel, collapsed)?;
            }
            LayoutCommand::AutoHide { panel, content } => next.auto_hide(&panel, content)?,
            LayoutCommand::Pin(panel) => next.pin(&panel)?,
            LayoutCommand::Switch(w) => {
                closed.extend(
                    before
                        .panels()
                        .into_iter()
                        .filter(|p| !w.is_open(p))
                        .cloned(),
                );
                next = w;
            }
        }
        // Every result is checked, so a sequence of valid commands can never
        // produce a workspace that fails to load again (for example by nesting
        // splits deeper than the file format allows).
        next.validate()?;
        if next == before {
            return Ok(None);
        }
        self.history.record(&before, group);
        if group.is_none() {
            self.history.seal();
        }
        self.workspace = next;
        let toast = if self.live.live {
            closed
                .into_iter()
                .find(|id| built_in(id).is_some_and(|p| p.guarded_while_live))
                .map(Toast::ClosedWhileLive)
        } else {
            None
        };
        Ok(toast)
    }

    /// Switch workspace because the stream started or ended (opt-in, rule U21).
    /// The switch is in the layout undo history like any other, and the toast
    /// names the previous workspace for the one-click way back (undo).
    pub fn switch_automatically(&mut self, to: Workspace) -> Result<Option<Toast>, CommandError> {
        // One toast covers it: its way back (undo) also reopens any guarded
        // panel the switch closed while live.
        let before = self.workspace.clone();
        self.apply(LayoutCommand::Switch(to), Origin::Command)?;
        if self.workspace == before {
            return Ok(None);
        }
        Ok(Some(Toast::SwitchedAutomatically {
            previous: before.name,
        }))
    }

    /// End the current undo group (for example when a splitter drag ends).
    pub fn end_group(&mut self) {
        self.history.seal();
    }

    /// Undo the last layout change. Never touches engine state.
    pub fn undo(&mut self) -> bool {
        self.history.undo(&mut self.workspace)
    }

    /// Is there a layout change to undo?
    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    /// Is there an undone layout change to redo?
    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// Redo the last undone layout change.
    pub fn redo(&mut self) -> bool {
        self.history.redo(&mut self.workspace)
    }

    /// How often the shell must redraw without input: the fastest rate of any
    /// visible panel, and once a second for the live indicator's duration while
    /// live or recording. [`UpdateRate::OnChange`] means no timer at all.
    pub fn redraw_rate(&self) -> UpdateRate {
        let panels = needed_rate(self.workspace.visible());
        if self.live.is_active() {
            panels.max(UpdateRate::Hz(1))
        } else {
            panels
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::builtin;
    use crate::panel::id;

    #[test]
    fn close_while_live_offers_undo() {
        let mut s = LayoutSession::new(builtin::minimal());
        s.set_live(LiveStatus {
            live: true,
            elapsed_secs: 5,
            outputs: vec!["Twitch".into()],
            ..LiveStatus::default()
        });
        let t = s
            .apply(
                LayoutCommand::ClosePanel(id("rbs.program")),
                Origin::Pointer,
            )
            .unwrap();
        assert_eq!(t, Some(Toast::ClosedWhileLive(id("rbs.program"))));
        assert!(s.undo());
        assert!(s.workspace().is_open(&id("rbs.program")));
        // Not guarded, or not live: no toast.
        let mut s = LayoutSession::new(builtin::edit());
        let t = s.apply(
            LayoutCommand::ClosePanel(id("rbs.program").clone()),
            Origin::Command,
        );
        assert_eq!(t, Ok(None), "not live");
        // Closing Program closes Edit's Pictures area (ADR-0073).
        assert!(s.workspace().main().pictures_leaf().is_none());
        assert_eq!(
            s.apply(LayoutCommand::ClosePanel(id("rbs.mixer")), Origin::Command),
            Ok(None)
        );
    }

    #[test]
    fn closing_a_floating_window_while_live_is_guarded_too() {
        let mut s = LayoutSession::new(builtin::live());
        s.apply(LayoutCommand::Float(id("rbs.mixer")), Origin::Command)
            .unwrap();
        s.set_live(LiveStatus {
            live: true,
            ..LiveStatus::default()
        });
        let t = s
            .apply(LayoutCommand::CloseWindow { window: 1 }, Origin::Command)
            .unwrap();
        assert_eq!(t, Some(Toast::ClosedWhileLive(id("rbs.mixer"))));
    }

    #[test]
    fn switch_while_live_reports_dropped_guarded_panels() {
        let mut s = LayoutSession::new(builtin::live());
        s.set_live(LiveStatus {
            live: true,
            ..LiveStatus::default()
        });
        let t = s
            .apply(LayoutCommand::Switch(builtin::minimal()), Origin::Command)
            .unwrap();
        assert_eq!(t, Some(Toast::ClosedWhileLive(id("rbs.mixer"))));
        // Edit holds Program in its studio split (ADR-0064): nothing guarded closes.
        let t = s
            .apply(LayoutCommand::Switch(builtin::edit()), Origin::Command)
            .unwrap();
        assert_eq!(t, None);
        let t = s.switch_automatically(builtin::minimal()).unwrap();
        assert_eq!(
            t,
            Some(Toast::SwitchedAutomatically {
                previous: "Edit".into()
            })
        );
        assert!(s.undo());
        assert_eq!(s.workspace().name, "Edit");
    }

    #[test]
    fn edge_moves_cannot_exceed_the_file_depth() {
        let mut s = LayoutSession::new(builtin::minimal());
        let edges = [Edge::Left, Edge::Top];
        for i in 0..40 {
            let _ = s.apply(
                LayoutCommand::MoveToEdge {
                    panel: id("rbs.program"),
                    window: 0,
                    edge: edges[i % 2],
                },
                Origin::Command,
            );
            let text = s.workspace().to_toml().unwrap();
            Workspace::from_toml(&text).unwrap();
        }
        // A layout deeper than the file allows is refused, never stored.
        let mut w = builtin::minimal();
        for i in 0..40 {
            let mut n = crate::layout::Node::tabs(vec![
                PanelId::parse(&format!("com.example.p{i}")).unwrap(),
            ]);
            std::mem::swap(&mut n, w.windows[0].layout_mut().unwrap());
            let axis = if i % 2 == 0 {
                crate::layout::Axis::Horizontal
            } else {
                crate::layout::Axis::Vertical
            };
            *w.windows[0].layout_mut().unwrap() = crate::layout::Node::split(
                axis,
                vec![(0.5, n), (0.5, w.windows[0].layout().unwrap().clone())],
            );
        }
        let mut s = LayoutSession::new(builtin::minimal());
        assert!(s.apply(LayoutCommand::Switch(w), Origin::Command).is_err());
    }

    #[test]
    fn lock_blocks_pointer_only() {
        let mut s = LayoutSession::new(builtin::edit());
        s.apply(LayoutCommand::SetLocked(true), Origin::Pointer)
            .unwrap();
        assert_eq!(
            s.apply(LayoutCommand::ClosePanel(id("rbs.stats")), Origin::Pointer),
            Err(CommandError::Locked)
        );
        // Collapsing hides without rearranging, so the lock allows it (ADR-0049);
        // auto-hide rearranges and is refused from the pointer.
        assert!(
            s.apply(
                LayoutCommand::SetCollapsed {
                    panel: id("rbs.properties"),
                    collapsed: true
                },
                Origin::Pointer
            )
            .is_ok()
        );
        assert_eq!(
            s.apply(
                LayoutCommand::AutoHide {
                    panel: id("rbs.properties"),
                    content: Rect::sized(1440.0, 800.0)
                },
                Origin::Pointer
            ),
            Err(CommandError::Locked)
        );
        assert_eq!(
            s.apply(LayoutCommand::Pin(id("rbs.stats")), Origin::Pointer),
            Err(CommandError::Locked)
        );
        assert!(
            s.apply(LayoutCommand::ClosePanel(id("rbs.stats")), Origin::Command)
                .is_ok()
        );
    }

    #[test]
    fn drag_is_one_undo_step() {
        let mut s = LayoutSession::new(builtin::minimal());
        let before = s.workspace().clone();
        for _ in 0..10 {
            s.apply(
                LayoutCommand::Resize {
                    window: 0,
                    area: Rect::sized(1280.0, 720.0),
                    handle: 0,
                    delta: -3.0,
                    group: Some(1),
                    studio_mode: false,
                },
                Origin::Pointer,
            )
            .unwrap();
        }
        s.end_group();
        assert_ne!(s.workspace(), &before);
        assert!(s.undo());
        assert_eq!(s.workspace(), &before);
        assert!(!s.undo());
        assert!(s.redo());
    }

    #[test]
    fn no_op_commands_leave_no_history() {
        let mut s = LayoutSession::new(builtin::edit());
        s.apply(LayoutCommand::OpenPanel(id("rbs.scenes")), Origin::Command)
            .unwrap();
        assert!(!s.undo());
    }

    #[test]
    fn switch_is_undoable_and_validated() {
        let mut s = LayoutSession::new(builtin::edit());
        s.apply(LayoutCommand::Switch(builtin::minimal()), Origin::Command)
            .unwrap();
        assert_eq!(s.workspace().name, "Minimal");
        let mut broken = builtin::audio();
        broken.windows.clear();
        assert!(
            s.apply(LayoutCommand::Switch(broken), Origin::Command)
                .is_err()
        );
        assert!(s.undo());
        assert_eq!(s.workspace().name, "Edit");
    }

    #[test]
    fn redraw_rate_follows_visible_panels_and_live_state() {
        let mut s = LayoutSession::new(builtin::minimal());
        assert_eq!(s.redraw_rate(), UpdateRate::Display);
        s.apply(
            LayoutCommand::ClosePanel(id("rbs.program")),
            Origin::Command,
        )
        .unwrap();
        assert_eq!(s.redraw_rate(), UpdateRate::OnChange);
        s.set_live(LiveStatus {
            recording: true,
            ..LiveStatus::default()
        });
        assert_eq!(s.redraw_rate(), UpdateRate::Hz(1));
        // A hidden tab does not count: Live shows Stats in front of Scenes.
        let mut s = LayoutSession::new(builtin::live());
        for p in ["rbs.preview", "rbs.mixer"] {
            s.apply(LayoutCommand::ClosePanel(id(p)), Origin::Command)
                .unwrap();
        }
        assert_eq!(s.redraw_rate(), UpdateRate::Hz(1), "stats tab is visible");
        let (w, stack, _) = s.workspace().find(&id("rbs.scenes")).unwrap();
        s.apply(
            LayoutCommand::ActivateTab {
                window: w,
                stack,
                tab: 1,
            },
            Origin::Command,
        )
        .unwrap();
        assert_eq!(s.redraw_rate(), UpdateRate::OnChange, "stats tab is hidden");
    }

    #[test]
    fn areas_show_hide_and_reset() {
        use crate::layout::Node;
        use crate::panel::id;
        use crate::workspace::Side;
        let mut ws = builtin::edit();
        {
            let m = ws.main_layout_mut().unwrap();
            m.left.layout = Some(Node::tabs(vec![id("rbs.status-bar")]));
            m.left.size = 400;
        }
        ws.locked = true;
        let mut s = LayoutSession::new(ws);
        // Showing and hiding rearrange nothing: allowed from the pointer while locked.
        s.apply(
            LayoutCommand::SetAreaVisible {
                side: Side::Left,
                visible: false,
            },
            Origin::Pointer,
        )
        .unwrap();
        let m = s.workspace().main_layout().unwrap();
        assert!(!m.left.visible && m.left.layout.is_some());
        assert!(!s.workspace().visible().contains(&&id("rbs.status-bar")));
        s.apply(LayoutCommand::SetAllAreasVisible(true), Origin::Pointer)
            .unwrap();
        assert!(s.workspace().main_layout().unwrap().left.visible);
        s.apply(LayoutCommand::ResetSizes, Origin::Command).unwrap();
        assert_eq!(s.workspace().main_layout().unwrap().left.size, 260);
        assert!(s.undo());
        assert_eq!(s.workspace().main_layout().unwrap().left.size, 400);
        // A toolbar edit rearranges: refused from the pointer while locked.
        assert_eq!(
            s.apply(LayoutCommand::ResetToolbar, Origin::Pointer),
            Err(CommandError::Locked)
        );
    }

    #[test]
    fn reset_workspace_brings_back_the_built_in_default() {
        // A saved Edit from before ADR-0064: no studio split.
        let mut old = builtin::edit();
        old.name = "edit".to_owned();
        old.close(&id("rbs.program")).unwrap();
        old.locked = true;
        assert_eq!(old.studio_rail(), None);
        let mut s = LayoutSession::new(old.clone());
        s.apply(LayoutCommand::ResetWorkspace, Origin::Command)
            .unwrap();
        let mut want = builtin::edit();
        want.name = "edit".to_owned();
        assert_eq!(s.workspace(), &want);
        assert!(s.workspace().studio_rail().is_some());
        // Once more changes nothing and adds no undo step.
        s.apply(LayoutCommand::ResetWorkspace, Origin::Command)
            .unwrap();
        assert!(s.undo());
        assert_eq!(s.workspace(), &old);
        assert!(!s.can_undo());
    }

    #[test]
    fn reset_workspace_refuses_a_workspace_that_is_not_built_in() {
        let mut ws = builtin::edit();
        ws.name = "My show".to_owned();
        let mut s = LayoutSession::new(ws);
        assert_eq!(
            s.apply(LayoutCommand::ResetWorkspace, Origin::Command),
            Err(CommandError::Workspace(WorkspaceError::NoDefault))
        );
    }
}
