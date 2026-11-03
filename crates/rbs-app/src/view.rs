//! Turns the rbs-ui model into the plain data the Slint views draw.

use rbs_ui::layout::{Axis, Flat, Node, Rect};
use rbs_ui::panel::{BUILT_IN_PANELS, PanelId, built_in};
use rbs_ui::{LiveState, LiveStatus, Workspace};
use slint::{ModelRc, SharedString, VecModel};

use crate::ui::{HandleData, PanelEntry, StackData, TabData, WorkspaceEntry};

/// English title of a panel; unknown panels show their id.
pub fn title(id: &PanelId) -> String {
    let base = built_in(id).map_or(id.as_str(), |p| p.title);
    match id.param("canvas") {
        Some(c) if built_in(id).is_some() => format!("{base} ({c})"),
        _ => base.to_owned(),
    }
}

/// Tab stacks and handles of one window, for its view.
pub fn window_view(window: usize, layout: &Node, area: Rect) -> (Vec<StackData>, Vec<HandleData>) {
    let Flat { stacks, handles } = layout.flatten(area);
    let stacks = stacks
        .into_iter()
        .map(|s| StackData {
            window: window as i32,
            stack: s.stack as i32,
            x: s.rect.x,
            y: s.rect.y,
            w: s.rect.w,
            h: s.rect.h,
            tabs: ModelRc::new(VecModel::from(
                s.tabs
                    .iter()
                    .map(|id| TabData {
                        id: id.as_str().into(),
                        title: title(id).into(),
                        known: built_in(id).is_some(),
                    })
                    .collect::<Vec<_>>(),
            )),
            active: s.active as i32,
        })
        .collect();
    let handles = handles
        .into_iter()
        .enumerate()
        .map(|(i, h)| HandleData {
            window: window as i32,
            index: i as i32,
            x: h.rect.x,
            y: h.rect.y,
            w: h.rect.w,
            h: h.rect.h,
            horizontal: h.axis == Axis::Horizontal,
            percent: percent_before(layout, &h),
        })
        .collect();
    (stacks, handles)
}

/// Share of the pane before a handle, in percent of the pair it divides.
fn percent_before(layout: &Node, h: &rbs_ui::layout::Handle) -> i32 {
    let mut node = layout;
    for &i in &h.path {
        match node {
            Node::Split(s) => match s.children.get(i) {
                Some(c) => node = c,
                None => return 0,
            },
            Node::Tabs(_) => return 0,
        }
    }
    match node {
        Node::Split(s) => {
            let a = s.ratios.get(h.index).copied().unwrap_or(0.0);
            let b = s.ratios.get(h.index + 1).copied().unwrap_or(0.0);
            if a + b > 0.0 {
                (100.0 * a / (a + b)).round() as i32
            } else {
                0
            }
        }
        Node::Tabs(_) => 0,
    }
}

/// The Panels menu: built-in panels, then any unknown panels that are open.
pub fn panel_entries(ws: &Workspace) -> Vec<PanelEntry> {
    let mut out: Vec<PanelEntry> = BUILT_IN_PANELS
        .iter()
        .filter_map(|p| PanelId::parse(p.id).ok())
        .map(|id| PanelEntry {
            open: ws.is_open(&id),
            title: title(&id).into(),
            id: id.as_str().into(),
        })
        .collect();
    for id in ws.panels() {
        if built_in(id).is_none() || id.param("canvas").is_some() {
            out.push(PanelEntry {
                id: id.as_str().into(),
                title: title(id).into(),
                open: true,
            });
        }
    }
    out
}

/// Workspace menu entries.
pub fn workspace_entries(names: &[(String, bool)], current: &str) -> Vec<WorkspaceEntry> {
    names
        .iter()
        .map(|(n, built_in)| WorkspaceEntry {
            name: n.as_str().into(),
            built_in: *built_in,
            current: rbs_ui::workspace::same_name(n, current),
        })
        .collect()
}

/// Live indicator state code used by the view (0 off, 1 live, 2 recording, 3 both).
pub fn live_code(live: &LiveStatus) -> i32 {
    match live.state() {
        LiveState::Off => 0,
        LiveState::Live => 1,
        LiveState::Recording => 2,
        LiveState::LiveAndRecording => 3,
    }
}

/// Names of a floating window's panels, for its title.
pub fn panel_names(layout: &Node) -> SharedString {
    let names: Vec<String> = layout.panels().into_iter().map(title).collect();
    names.join(", ").into()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use rbs_ui::builtin;
    use slint::Model;

    #[test]
    fn edit_workspace_view() {
        let ws = builtin::edit();
        let (stacks, handles) = window_view(0, ws.main(), Rect::sized(1440.0, 860.0));
        assert_eq!(stacks.len(), ws.main().stack_count());
        assert!(!handles.is_empty());
        assert!(handles.iter().all(|h| (0..=100).contains(&h.percent)));
        let mixer = stacks
            .iter()
            .find(|s| s.tabs.iter().any(|t| t.id == "rbs.mixer"))
            .unwrap();
        assert_eq!(mixer.tabs.row_count(), 3);
        assert!(mixer.tabs.iter().all(|t| t.known));
    }

    #[test]
    fn titles_and_menu() {
        let p = PanelId::parse("rbs.program?canvas=vertical").unwrap();
        assert_eq!(title(&p), "Program (vertical)");
        let plugin = PanelId::parse("com.example.chat").unwrap();
        assert_eq!(title(&plugin), "com.example.chat");
        let mut ws = builtin::minimal();
        ws.open(plugin).unwrap();
        let entries = panel_entries(&ws);
        assert_eq!(entries.len(), BUILT_IN_PANELS.len() + 1);
        assert!(entries.iter().any(|e| e.id == "rbs.program" && e.open));
        assert!(entries.iter().any(|e| e.id == "rbs.mixer" && !e.open));
    }

    #[test]
    fn live_codes() {
        let mut l = LiveStatus::default();
        assert_eq!(live_code(&l), 0);
        l.live = true;
        l.recording = true;
        assert_eq!(live_code(&l), 3);
    }
}
