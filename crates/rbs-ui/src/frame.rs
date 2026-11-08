//! Where the main window's centre and areas go, and a whole window flattened into
//! tab stacks and dividers (ADR-0049 decisions 1 and 3).
//!
//! The bottom area spans the width under the left area, the centre and the right
//! area. An area takes space only while it is shown and holds panels. Sizes are the
//! areas' own, clamped to their limits (spec, Size limits), and when the window is too
//! small the centre keeps its minimum first and the side areas shrink in proportion.
//! Collapsing stacks and drawing areas as edge tabs when even their minimums do not
//! fit come with stack states.

use crate::layout::{
    Axis, COLLAPSED_STACK, EDGE_SHARE, Edge, HANDLE_THICKNESS, Handle, Rect, StackRect,
};
use crate::panel::PanelId;
use crate::workspace::{MainLayout, Side, Window, WindowContent};

/// The centre's smallest width and height in logical pixels (one preview).
pub const CENTRE_MIN: (f32, f32) = (320.0, 180.0);

/// An area's smallest size: width for left and right, height for bottom.
pub fn min_size(side: Side) -> f32 {
    match side {
        Side::Left | Side::Right => 200.0,
        Side::Bottom => 90.0,
    }
}

/// An area's largest size as a share of the window's content.
pub fn max_share(side: Side) -> f32 {
    match side {
        Side::Left | Side::Right => 0.4,
        Side::Bottom => 0.6,
    }
}

/// Is the area on `side` laid out: shown and holding panels?
pub fn shown(m: &MainLayout, side: Side) -> bool {
    let a = m.area(side);
    a.visible && a.layout.is_some()
}

/// The size an area is drawn at, from its own size and limits alone.
fn limited(side: Side, size: f32, content: Rect) -> f32 {
    let along = match side {
        Side::Left | Side::Right => content.w,
        Side::Bottom => content.h,
    };
    size.min(along * max_share(side)).max(min_size(side))
}

/// Rectangles of the main window's content.
#[derive(Clone, Debug, PartialEq)]
pub struct MainRects {
    /// The centre.
    pub centre: Rect,
    /// Each area that is laid out, with its rectangle.
    pub areas: Vec<(Side, Rect)>,
    /// The divider between each laid out area and the rest.
    pub dividers: Vec<(Side, Rect)>,
}

impl MainRects {
    /// The rectangle of the area on `side`, if it is laid out.
    pub fn area(&self, side: Side) -> Option<Rect> {
        self.areas.iter().find(|(s, _)| *s == side).map(|(_, r)| *r)
    }
}

/// Lay out the centre and areas of `m` in `content`.
pub fn main_rects(m: &MainLayout, content: Rect) -> MainRects {
    let t = HANDLE_THICKNESS;
    let mut out = MainRects {
        centre: content,
        areas: Vec::new(),
        dividers: Vec::new(),
    };
    // Bottom first: it spans the full width.
    let mut upper_h = content.h;
    if shown(m, Side::Bottom) {
        let wanted = limited(Side::Bottom, m.bottom.size as f32, content);
        let h = wanted.min(content.h - CENTRE_MIN.1 - t).max(0.0);
        upper_h = (content.h - h - t).max(0.0);
        out.dividers.push((
            Side::Bottom,
            Rect {
                x: content.x,
                y: content.y + upper_h,
                w: content.w,
                h: t,
            },
        ));
        out.areas.push((
            Side::Bottom,
            Rect {
                x: content.x,
                y: content.y + upper_h + t,
                w: content.w,
                h,
            },
        ));
    }
    let sides: Vec<Side> = [Side::Left, Side::Right]
        .into_iter()
        .filter(|s| shown(m, *s))
        .collect();
    let mut widths: Vec<f32> = sides
        .iter()
        .map(|s| limited(*s, m.area(*s).size as f32, content))
        .collect();
    let avail = (content.w - CENTRE_MIN.0 - t * sides.len() as f32).max(0.0);
    let total: f32 = widths.iter().sum();
    if total > avail && total > 0.0 {
        let k = avail / total;
        widths.iter_mut().for_each(|w| *w *= k);
    }
    let mut left = content.x;
    let mut right = content.x + content.w;
    for (side, w) in sides.into_iter().zip(widths) {
        let (area_x, divider_x) = match side {
            Side::Left => {
                let x = left;
                left += w + t;
                (x, x + w)
            }
            _ => {
                right -= w + t;
                (right + t, right)
            }
        };
        out.areas.push((
            side,
            Rect {
                x: area_x,
                y: content.y,
                w,
                h: upper_h,
            },
        ));
        out.dividers.push((
            side,
            Rect {
                x: divider_x,
                y: content.y,
                w: t,
                h: upper_h,
            },
        ));
    }
    out.centre = Rect {
        x: left,
        y: content.y,
        w: (right - left).max(0.0),
        h: upper_h,
    };
    // Areas in stack numbering order, so callers can zip them with trees.
    out.areas
        .sort_by_key(|(s, _)| Side::ALL.iter().position(|x| x == s));
    out.dividers
        .sort_by_key(|(s, _)| Side::ALL.iter().position(|x| x == s));
    out
}

/// What a divider of a window moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleTarget {
    /// A splitter in the window's tree number `n` ([`Window::trees`] order).
    Tree(usize),
    /// The edge of an area of the main window.
    Area(Side),
}

/// A divider of a window.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowHandle {
    /// What it moves.
    pub target: HandleTarget,
    /// Where it is; for a tree splitter, the tree's own handle.
    pub handle: Handle,
}

/// A whole window as absolute rectangles: every tree that is shown, with window stack
/// numbers ([`Window::stack`]), and every divider.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WindowFlat {
    /// Tab stacks that are shown; `stack` is the window stack index.
    pub stacks: Vec<StackRect>,
    /// Dividers: the trees' splitters, then the areas' edges.
    pub handles: Vec<WindowHandle>,
    /// The Transition rail of the centre's studio split in studio mode (ADR-0064).
    pub rail: Option<(Rect, crate::layout::RailAt)>,
}

/// Flatten window `w` laid out in `content`.
pub fn flatten_window(w: &Window, content: Rect, studio_mode: bool) -> WindowFlat {
    let mut out = WindowFlat::default();
    let mut place = |tree: usize, first: usize, node: &crate::layout::Node, r: Rect| {
        let flat = node.flatten_in(r, studio_mode);
        if tree == 0 && out.rail.is_none() {
            out.rail = flat.rail;
        }
        out.stacks.extend(flat.stacks.into_iter().map(|mut s| {
            // The Pictures leaf's pictures keep their own numbers (ADR-0073).
            if s.stack < crate::layout::PICTURES_PREVIEW_STACK {
                s.stack += first;
            }
            s
        }));
        out.handles
            .extend(flat.handles.into_iter().map(|handle| WindowHandle {
                target: HandleTarget::Tree(tree),
                handle,
            }));
    };
    match &w.content {
        WindowContent::Main(m) => {
            let rects = main_rects(m, content);
            place(0, 0, &m.centre, rects.centre);
            let mut tree = 1;
            let mut first = m.centre.stack_count();
            for side in Side::ALL {
                let Some(layout) = &m.area(side).layout else {
                    continue;
                };
                // A hidden area keeps its stack numbers.
                if let Some(r) = rects.area(side) {
                    place(tree, first, layout, r);
                }
                tree += 1;
                first += layout.stack_count();
            }
            for (side, rect) in rects.dividers {
                let (axis, extent) = match side {
                    Side::Bottom => (Axis::Vertical, content.h),
                    _ => (Axis::Horizontal, content.w),
                };
                out.handles.push(WindowHandle {
                    target: HandleTarget::Area(side),
                    handle: Handle {
                        path: Vec::new(),
                        index: 0,
                        axis,
                        rect,
                        extent,
                    },
                });
            }
        }
        WindowContent::Floating(n) => place(0, 0, n, content),
        WindowContent::Projector(_) => {}
    }
    out
}

/// The new size of the area on `side` after its divider moved by `delta` pixels
/// (right or down is positive), within its limits and the centre's minimum.
pub fn resized_area(m: &MainLayout, content: Rect, side: Side, delta: f32) -> u32 {
    let rects = main_rects(m, content);
    let (current, room) = match (side, rects.area(side)) {
        (Side::Bottom, Some(r)) => (r.h, rects.centre.h - CENTRE_MIN.1),
        (_, Some(r)) => (r.w, rects.centre.w - CENTRE_MIN.0),
        (_, None) => return m.area(side).size,
    };
    let grow = match side {
        Side::Left => delta,
        Side::Right | Side::Bottom => -delta,
    };
    let wanted = current + grow.min(room.max(0.0));
    limited(side, wanted, content).round() as u32
}

/// The window as shown while panel `panel` is maximized (ADR-0049 decision 2): the
/// stack holding it alone over the whole content, with the panel as its shown tab, and
/// no dividers. `None` when no stack shown in this window holds the panel.
pub fn maximized(flat: &WindowFlat, content: Rect, panel: &PanelId) -> Option<WindowFlat> {
    let s = flat.stacks.iter().find(|s| s.tabs.contains(panel))?;
    let active = s.tabs.iter().position(|p| p == panel)?;
    Some(WindowFlat {
        stacks: vec![StackRect {
            stack: s.stack,
            rect: content,
            tabs: s.tabs.clone(),
            active,
            collapsed: false,
        }],
        handles: Vec::new(),
        rail: None,
    })
}

/// How close to a window's edge, in logical pixels, a drop goes to that edge of the
/// window rather than into the stack under the pointer.
pub const EDGE_TARGET: f32 = 16.0;

/// The share of a stack's width or height, from each side, that drops beside the
/// stack on that side; the middle drops into it as a tab.
pub const SIDE_SHARE: f32 = 0.3;

/// The widest guide a window-edge drop shows, in logical pixels.
const EDGE_GUIDE_MAX: f32 = 320.0;

/// Where a dragged panel would go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropPlace {
    /// Into window stack `stack` as a tab (the middle of the five-way guide).
    Into(usize),
    /// Into a new stack on side `edge` of window stack `stack`.
    Beside(usize, Edge),
    /// To edge `edge` of the window (in the main window, that area's outer side).
    WindowEdge(Edge),
}

/// A drop target: where the panel goes and the rectangle the shell shades for it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DropTarget {
    /// Where the panel goes.
    pub place: DropPlace,
    /// The part of the window the drop will fill, roughly.
    pub guide: Rect,
}

/// The drop target for panel `panel` dragged to `(x, y)` over a window flattened into
/// `flat` in `content` (ADR-0049 decision 5): one of the four window-edge targets near
/// the window's edges, else the five-way guide of the stack under the pointer. The
/// title row of a stack, and all of a collapsed one, take the panel as a tab. `None`
/// when the drop would change nothing or nothing is under the pointer.
pub fn drop_target(
    flat: &WindowFlat,
    content: Rect,
    panel: &PanelId,
    x: f32,
    y: f32,
) -> Option<DropTarget> {
    let inside = |r: &Rect| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
    if !inside(&content) {
        return None;
    }
    let under = flat.stacks.iter().find(|s| inside(&s.rect));
    // A stack's title row takes the panel as a tab even at the window's edge.
    let title = under.is_some_and(|s| s.collapsed || y < s.rect.y + COLLAPSED_STACK);
    let edges = [
        (Edge::Left, x - content.x),
        (Edge::Right, content.x + content.w - x),
        (Edge::Top, y - content.y),
        (Edge::Bottom, content.y + content.h - y),
    ];
    let near = edges
        .iter()
        .filter(|(_, d)| *d < EDGE_TARGET)
        .min_by(|a, b| a.1.total_cmp(&b.1));
    if let (Some((edge, _)), false) = (near, title) {
        return Some(DropTarget {
            place: DropPlace::WindowEdge(*edge),
            guide: side_of(content, *edge, |extent| {
                (extent * EDGE_SHARE).min(EDGE_GUIDE_MAX)
            }),
        });
    }
    let s = under?;
    // The Pictures leaf takes no tab and has no sides to drop beside (ADR-0073).
    if s.stack >= crate::layout::PICTURES_PREVIEW_STACK {
        return None;
    }
    let r = s.rect;
    let own = s.tabs.contains(panel);
    let side = if title {
        None
    } else {
        let fx = (x - r.x) / r.w;
        let fy = (y - r.y) / r.h;
        [
            (Edge::Left, fx),
            (Edge::Right, 1.0 - fx),
            (Edge::Top, fy),
            (Edge::Bottom, 1.0 - fy),
        ]
        .into_iter()
        .filter(|(_, d)| *d < SIDE_SHARE)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(e, _)| e)
    };
    match side {
        // Its own stack, as a tab or beside itself when it is the only tab.
        None if own => None,
        Some(_) if own && s.tabs.len() == 1 => None,
        None => Some(DropTarget {
            place: DropPlace::Into(s.stack),
            guide: r,
        }),
        Some(edge) => Some(DropTarget {
            place: DropPlace::Beside(s.stack, edge),
            guide: side_of(r, edge, |extent| extent / 2.0),
        }),
    }
}

/// The strip along side `edge` of `r`, as thick as `thickness` gives for the extent
/// across that side.
fn side_of(r: Rect, edge: Edge, thickness: impl Fn(f32) -> f32) -> Rect {
    match edge {
        Edge::Left => Rect {
            w: thickness(r.w),
            ..r
        },
        Edge::Right => {
            let w = thickness(r.w);
            Rect {
                x: r.x + r.w - w,
                w,
                ..r
            }
        }
        Edge::Top => Rect {
            h: thickness(r.h),
            ..r
        },
        Edge::Bottom => {
            let h = thickness(r.h);
            Rect {
                y: r.y + r.h - h,
                h,
                ..r
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::layout::Node;
    use crate::panel::PanelId;

    fn tabs(ids: &[&str]) -> Node {
        Node::tabs(ids.iter().filter_map(|i| PanelId::parse(i).ok()).collect())
    }

    fn layout(left: bool, right: bool, bottom: bool) -> MainLayout {
        let mut m = MainLayout::new(tabs(&["rbs.preview"]));
        if left {
            m.left.layout = Some(tabs(&["rbs.scenes", "rbs.sources"]));
        }
        if right {
            m.right.layout = Some(tabs(&["rbs.properties"]));
        }
        if bottom {
            m.bottom.layout = Some(tabs(&["rbs.mixer"]));
        }
        m
    }

    const WINDOW: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 1280.0,
        h: 720.0,
    };

    #[test]
    fn empty_areas_take_no_space() {
        let r = main_rects(&layout(false, false, false), WINDOW);
        assert_eq!(r.centre, WINDOW);
        assert!(r.areas.is_empty() && r.dividers.is_empty());
    }

    #[test]
    fn areas_keep_their_sizes_and_the_bottom_spans_the_width() {
        let r = main_rects(&layout(true, true, true), WINDOW);
        let t = HANDLE_THICKNESS;
        assert_eq!(r.area(Side::Left).map(|a| a.w), Some(260.0));
        assert_eq!(r.area(Side::Right).map(|a| a.w), Some(260.0));
        let bottom = r.area(Side::Bottom).expect("bottom");
        assert_eq!((bottom.x, bottom.w, bottom.h), (0.0, 1280.0, 200.0));
        assert_eq!(r.centre.x, 260.0 + t);
        assert_eq!(r.centre.w, 1280.0 - 2.0 * (260.0 + t));
        assert_eq!(r.centre.h, 720.0 - 200.0 - t);
        assert_eq!(r.dividers.len(), 3);
    }

    #[test]
    fn hidden_areas_take_no_space() {
        let mut m = layout(true, false, false);
        m.left.visible = false;
        assert_eq!(main_rects(&m, WINDOW).centre, WINDOW);
    }

    #[test]
    fn the_centre_keeps_its_minimum_in_a_small_window() {
        let mut m = layout(true, true, true);
        m.left.size = 2000;
        m.right.size = 2000;
        m.bottom.size = 2000;
        for (w, h) in [
            (640.0, 360.0),
            (800.0, 450.0),
            (1280.0, 720.0),
            (300.0, 150.0),
        ] {
            let content = Rect::sized(w, h);
            let r = main_rects(&m, content);
            if w >= CENTRE_MIN.0 + 2.0 * HANDLE_THICKNESS {
                assert!(r.centre.w + 0.01 >= CENTRE_MIN.0, "{w}x{h}: {:?}", r.centre);
            }
            if h >= CENTRE_MIN.1 + HANDLE_THICKNESS {
                assert!(r.centre.h + 0.01 >= CENTRE_MIN.1, "{w}x{h}: {:?}", r.centre);
            }
            // Nothing leaves the window.
            for (_, a) in &r.areas {
                assert!(a.x >= -0.01 && a.x + a.w <= w + 0.01, "{a:?}");
                assert!(a.y >= -0.01 && a.y + a.h <= h + 0.01, "{a:?}");
            }
        }
    }

    #[test]
    fn a_window_numbers_stacks_through_its_trees_and_skips_hidden_ones() {
        let mut m = layout(true, true, false);
        m.left.layout = Some(Node::Split(crate::layout::Split {
            axis: Axis::Vertical,
            ratios: vec![0.5, 0.5],
            children: vec![tabs(&["rbs.scenes"]), tabs(&["rbs.sources"])],
        }));
        let mut w = Window {
            content: WindowContent::Main(Box::new(m)),
            placement: None,
        };
        let flat = flatten_window(&w, WINDOW, false);
        let numbers: Vec<usize> = flat.stacks.iter().map(|s| s.stack).collect();
        assert_eq!(numbers, vec![0, 1, 2, 3]);
        // The left area's splitter, then the two area edges.
        let targets: Vec<HandleTarget> = flat.handles.iter().map(|h| h.target).collect();
        assert_eq!(
            targets,
            vec![
                HandleTarget::Tree(1),
                HandleTarget::Area(Side::Left),
                HandleTarget::Area(Side::Right)
            ]
        );
        if let Some(m) = w.main_mut() {
            m.left.visible = false;
        }
        let flat = flatten_window(&w, WINDOW, false);
        let numbers: Vec<usize> = flat.stacks.iter().map(|s| s.stack).collect();
        assert_eq!(numbers, vec![0, 3]);
    }

    #[test]
    fn dragging_an_area_edge_changes_its_size_within_limits() {
        let m = layout(true, true, true);
        assert_eq!(resized_area(&m, WINDOW, Side::Left, 40.0), 300);
        assert_eq!(resized_area(&m, WINDOW, Side::Right, 40.0), 220);
        assert_eq!(resized_area(&m, WINDOW, Side::Bottom, -30.0), 230);
        // Not below the minimum, not past 40 % of the window.
        assert_eq!(resized_area(&m, WINDOW, Side::Left, -500.0), 200);
        assert_eq!(resized_area(&m, WINDOW, Side::Left, 400.0), 512);
    }

    #[test]
    fn drop_targets_follow_the_five_way_guide_and_the_window_edges() {
        let w = Window {
            content: WindowContent::Main(Box::new(layout(true, true, true))),
            placement: None,
        };
        let flat = flatten_window(&w, WINDOW, false);
        let preview = flat
            .stacks
            .iter()
            .find(|s| s.tabs.contains(&PanelId::parse("rbs.preview").expect("id")))
            .expect("preview")
            .clone();
        let r = preview.rect;
        let mixer = PanelId::parse("rbs.mixer").expect("id");
        let at = |fx: f32, fy: f32| {
            drop_target(&flat, WINDOW, &mixer, r.x + r.w * fx, r.y + r.h * fy).map(|t| t.place)
        };
        assert_eq!(at(0.5, 0.5), Some(DropPlace::Into(preview.stack)));
        assert_eq!(
            at(0.1, 0.5),
            Some(DropPlace::Beside(preview.stack, Edge::Left))
        );
        assert_eq!(
            at(0.9, 0.5),
            Some(DropPlace::Beside(preview.stack, Edge::Right))
        );
        assert_eq!(
            at(0.5, 0.2),
            Some(DropPlace::Beside(preview.stack, Edge::Top))
        );
        assert_eq!(
            at(0.5, 0.9),
            Some(DropPlace::Beside(preview.stack, Edge::Bottom))
        );
        // The title row takes it as a tab.
        let title = drop_target(&flat, WINDOW, &mixer, r.x + r.w * 0.5, r.y + 4.0);
        assert_eq!(title.map(|t| t.place), Some(DropPlace::Into(preview.stack)));
        // A drop beside shades that half of the stack.
        let left = drop_target(&flat, WINDOW, &mixer, r.x + 30.0, r.y + r.h * 0.5).expect("left");
        assert_eq!(left.guide, Rect { w: r.w / 2.0, ..r });
        // Near the window's edges: the window edge.
        let edge = drop_target(&flat, WINDOW, &mixer, 5.0, WINDOW.h / 2.0).expect("edge");
        assert_eq!(edge.place, DropPlace::WindowEdge(Edge::Left));
        assert_eq!(edge.guide.x, 0.0);
        assert!(edge.guide.w <= EDGE_GUIDE_MAX);
        let edge = drop_target(&flat, WINDOW, &mixer, WINDOW.w / 2.0, WINDOW.h - 2.0);
        assert_eq!(
            edge.map(|t| t.place),
            Some(DropPlace::WindowEdge(Edge::Bottom))
        );
        // Outside the window: nothing.
        assert_eq!(drop_target(&flat, WINDOW, &mixer, -1.0, 10.0), None);
        // Its own stack: no change as a tab, and none beside itself as the only tab.
        let preview_id = PanelId::parse("rbs.preview").expect("id");
        let own =
            |fx: f32| drop_target(&flat, WINDOW, &preview_id, r.x + r.w * fx, r.y + r.h * 0.5);
        assert_eq!(own(0.5), None);
        assert_eq!(own(0.1), None);
        // Beside its own stack when the stack has other tabs.
        let sources = PanelId::parse("rbs.sources").expect("id");
        let left_area = flat
            .stacks
            .iter()
            .find(|s| s.tabs.contains(&sources))
            .expect("left")
            .rect;
        let t = drop_target(
            &flat,
            WINDOW,
            &sources,
            left_area.x + left_area.w * 0.5,
            left_area.y + left_area.h * 0.9,
        );
        assert!(matches!(
            t.map(|t| t.place),
            Some(DropPlace::Beside(_, Edge::Bottom))
        ));
    }

    #[test]
    fn a_maximized_panel_fills_the_window_alone() {
        let w = Window {
            content: WindowContent::Main(Box::new(layout(true, true, true))),
            placement: None,
        };
        let flat = flatten_window(&w, WINDOW, false);
        let sources = PanelId::parse("rbs.sources").expect("id");
        let m = maximized(&flat, WINDOW, &sources).expect("shown");
        assert_eq!(m.stacks.len(), 1);
        assert_eq!(m.stacks[0].rect, WINDOW);
        assert_eq!(m.stacks[0].tabs[m.stacks[0].active], sources);
        assert!(m.handles.is_empty());
        let closed = PanelId::parse("rbs.stats").expect("id");
        assert_eq!(maximized(&flat, WINDOW, &closed), None);
    }
}
