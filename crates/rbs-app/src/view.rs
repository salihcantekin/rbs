//! Turns the rbs-ui model into the plain data the Slint views draw.

use rbs_ui::frame::{HandleTarget, WindowFlat};
use rbs_ui::layout::{Axis, Node, RailAt, Rect};
use rbs_ui::panel::{BUILT_IN_PANELS, PanelId, built_in};
use rbs_ui::workspace::{Side, Window};
use rbs_ui::{LiveState, LiveStatus, Workspace};
use slint::{ModelRc, SharedString, VecModel};

use crate::ui::{
    EdgeTabData, HandleData, PanelEntry, RailData, StackData, TabData, WorkspaceEntry,
};

/// English title of a panel; unknown panels show their id.
pub fn title(id: &PanelId) -> String {
    let base = built_in(id).map_or(id.as_str(), |p| p.title);
    match id.param("canvas") {
        Some(c) if built_in(id).is_some() => format!("{base} ({c})"),
        _ => base.to_owned(),
    }
}

/// The icon of a panel in edge tabs and collapsed strips (design system icon set).
pub fn panel_icon(id: &PanelId) -> &'static str {
    match id.base() {
        "rbs.stats" => "grid",
        "rbs.outputs" => "copy",
        "rbs.library" => "image",
        _ => "window",
    }
}

/// The main window's edge tabs on the left, right and bottom edges, in file order.
pub fn edge_tabs(win: &Window) -> [Vec<EdgeTabData>; 3] {
    let mut out: [Vec<EdgeTabData>; 3] = Default::default();
    if let Some(m) = win.main() {
        for g in &m.auto_hide {
            let edge = match g.edge {
                Side::Left => 0,
                Side::Right => 1,
                Side::Bottom => 2,
            };
            out[edge].extend(g.tabs.iter().map(|id| EdgeTabData {
                id: id.as_str().into(),
                title: title(id).into(),
                icon: panel_icon(id).into(),
                size: g.size as f32,
            }));
        }
    }
    out
}

/// Tab stacks, dividers and the Transition rail (ADR-0064) of one window, for its
/// view, with the canvas in studio mode or not.
pub fn window_view(
    window: usize,
    win: &Window,
    area: Rect,
    maximized: Option<&PanelId>,
    studio_mode: bool,
) -> (Vec<StackData>, Vec<HandleData>, RailData) {
    let WindowFlat {
        stacks,
        handles,
        rail,
        header,
    } = shown_flat(win, area, maximized, studio_mode);
    let mut rail = rail.map_or_else(RailData::default, |(r, at)| RailData {
        shown: true,
        x: r.x,
        y: r.y,
        w: r.w,
        h: r.h,
        below: at == RailAt::Below,
        ..RailData::default()
    });
    // The Pictures leaf's header strip with the Studio mode switch (ADR-0073).
    if let Some(h) = header {
        rail.strip = true;
        rail.strip_x = h.x;
        rail.strip_y = h.y;
        rail.strip_w = h.w;
        rail.strip_h = h.h;
    }
    let trees = win.trees();
    // The main window's centre keeps one expanded stack and a panel (ADR-0049): its
    // last expanded stack offers no Collapse, and a centre of one stack no Auto-hide.
    let centre = win.main().map(|m| &m.centre);
    let centre_stacks = centre.map_or(0, Node::stack_count);
    let centre_expanded = centre.map_or(0, |c| {
        (0..centre_stacks)
            .filter(|&i| c.stack(i).is_some_and(|t| !t.collapsed))
            .count()
    });
    let lone_centre = centre.is_some_and(|c| matches!(c, Node::Tabs(_)));
    // Preview and Program in the studio split are not collapsed or auto-hidden
    // (ADR-0064 decision 4).
    let in_studio =
        |s: &rbs_ui::layout::StackRect| s.stack >= rbs_ui::layout::PICTURES_PREVIEW_STACK;
    let stacks = stacks
        .into_iter()
        .map(|s| StackData {
            can_collapse: !in_studio(&s)
                && (s.collapsed || s.stack >= centre_stacks || centre_expanded > 1),
            can_hide: win.is_main() && !in_studio(&s) && !(s.stack < centre_stacks && lone_centre),
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
                        icon: panel_icon(id).into(),
                    })
                    .collect::<Vec<_>>(),
            )),
            active: s.active as i32,
            collapsed: s.collapsed,
        })
        .collect();
    let handles = handles
        .into_iter()
        .enumerate()
        .map(|(i, h)| {
            let percent = match h.target {
                HandleTarget::Tree(t) => trees.get(t).map_or(0, |l| percent_before(l, &h.handle)),
                // An area's edge: its size as a share of the window's content.
                HandleTarget::Area(side) => win.main().map_or(0, |m| {
                    let size = m.area(side).size as f32;
                    (100.0 * size / h.handle.extent.max(1.0))
                        .round()
                        .clamp(0.0, 100.0) as i32
                }),
            };
            HandleData {
                window: window as i32,
                index: i as i32,
                x: h.handle.rect.x,
                y: h.handle.rect.y,
                w: h.handle.rect.w,
                h: h.handle.rect.h,
                horizontal: h.handle.axis == Axis::Horizontal,
                percent,
            }
        })
        .collect();
    (stacks, handles, rail)
}

/// Window `win` laid out in `area` as it is shown: with panel `maximized` alone over
/// the whole content when the window holds it (ADR-0049 decision 2).
pub fn shown_flat(
    win: &Window,
    area: Rect,
    maximized: Option<&PanelId>,
    studio_mode: bool,
) -> WindowFlat {
    let flat = win.flatten_in(area, studio_mode);
    maximized
        .and_then(|p| rbs_ui::frame::maximized(&flat, area, p))
        .unwrap_or(flat)
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
            // A rail edge: the rail's width within its limits.
            Node::Pictures(p) => {
                let (lo, hi) = rbs_ui::layout::RAIL_SIZE_RANGE;
                let share = (p.rail_size.clamp(lo, hi) - lo) as f32 / (hi - lo) as f32;
                return (100.0 * share).round() as i32;
            }
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
        Node::Tabs(_) | Node::Pictures(_) => 0,
    }
}

/// The Panels menu: built-in panels, then any unknown panels that are open.
pub fn panel_entries(ws: &Workspace) -> Vec<PanelEntry> {
    let mut out: Vec<PanelEntry> = BUILT_IN_PANELS
        .iter()
        .filter_map(|p| PanelId::parse(p.id).ok())
        // Transition is the studio rail beside the pictures (ADR-0064), listed only
        // where an older workspace still has it as a panel.
        .filter(|id| id.as_str() != "rbs.transition" || ws.is_open(id))
        // Go live and Record live in the window frame (ADR-0073), so the Live controls
        // panel is listed only where an older workspace still has it open.
        .filter(|id| id.as_str() != "rbs.live-controls" || ws.is_open(id))
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
        let (stacks, handles, rail) =
            window_view(0, &ws.windows[0], Rect::sized(1440.0, 860.0), None, false);
        assert!(!rail.shown, "no rail outside studio mode");
        // Program is hidden outside studio mode; the Pictures leaf is no stack but
        // reports its pictures (ADR-0073).
        assert_eq!(stacks.len(), ws.windows[0].stack_count() + 1);
        assert!(
            !stacks
                .iter()
                .any(|s| s.tabs.iter().any(|t| t.id == "rbs.program"))
        );
        let (on, _, rail) = window_view(0, &ws.windows[0], Rect::sized(1440.0, 860.0), None, true);
        assert_eq!(on.len(), ws.windows[0].stack_count() + 2);
        assert!(rail.shown && !rail.below);
        assert!(!handles.is_empty());
        assert!(handles.iter().all(|h| (0..=100).contains(&h.percent)));
        let mixer = stacks
            .iter()
            .find(|s| s.tabs.iter().any(|t| t.id == "rbs.mixer"))
            .unwrap();
        assert_eq!(mixer.tabs.row_count(), 1);
        let [left, right, bottom] = edge_tabs(&ws.windows[0]);
        assert!(bottom.is_empty());
        // The Library is the left edge tab (ADR-0075).
        assert_eq!(left.len(), 1);
        assert_eq!(right.len(), 2);
        assert!(
            !stacks
                .iter()
                .any(|s| s.tabs.iter().any(|t| t.id == "rbs.live-controls"))
        );
        let preview = stacks
            .iter()
            .find(|s| s.tabs.iter().any(|t| t.id == "rbs.preview"))
            .unwrap();
        assert!(!preview.can_collapse && !preview.can_hide);
        assert!(mixer.can_collapse && mixer.can_hide);
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
        // Every built-in panel but Transition and Live controls, and the plugin's.
        assert_eq!(entries.len(), BUILT_IN_PANELS.len() - 1);
        assert!(!entries.iter().any(|e| e.id == "rbs.transition"));
        assert!(!entries.iter().any(|e| e.id == "rbs.live-controls"));
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
