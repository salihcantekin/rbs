//! Toolkit-neutral model of the RBS desktop UI.
//!
//! This crate is the part of the desktop UI that does not depend on the UI
//! toolkit (ADR-0022, decision 4). The Slint shell in `rbs-app` only renders it,
//! so replacing the toolkit leaves this crate unchanged. It knows nothing about
//! the engine either: layout is client-side UI state (ADR-0017).
//!
//! - [`panel`]: panel ids, built-in panels and how often each needs redrawing.
//! - [`layout`]: a window's tree of splits and tab stacks, flattened to rectangles.
//! - [`workspace`]: the versioned workspace file and the layout operations of rule U21.
//! - [`command`]: the command registry behind menus, toolbar and palette (ADR-0050).
//! - [`builtin`]: the Edit, Live, Minimal and Audio workspaces.
//! - [`display`]: placing saved windows on the displays connected now.
//! - [`store`]: workspace files on disk.
//! - [`history`] and [`session`]: layout undo and the commands that change a layout.
//! - [`live`]: the live indicator.
//! - [`tokens`]: design tokens and their contrast checks.
//! - [`meter`]: the audio meter's scale, peak hold and clip latch.
//! - [`shortcut`]: key chords bound to commands, defaults, conflicts (ADR-0060).
//!
//! Spec: `docs/specs/ui-design-system.md`. Rules: `docs/ui/principles.md`.

pub mod builtin;
pub mod command;
pub mod display;
pub mod fader;
pub mod frame;
pub mod history;
pub mod layout;
pub mod live;
pub mod meter;
pub mod panel;
pub mod session;
pub mod shortcut;
pub mod store;
pub mod tokens;
pub mod transform;
pub mod workspace;

pub use layout::{Axis, Edge, Node, Rect};
pub use live::{LiveState, LiveStatus};
pub use panel::{PanelId, UpdateRate};
pub use session::{LayoutCommand, LayoutSession, Origin, Toast};
pub use workspace::{Window, WindowContent, Workspace, WorkspaceError};
