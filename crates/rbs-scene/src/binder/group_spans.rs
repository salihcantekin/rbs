//! Group spans (ADR-0185, ADR-0193): the drawn members of a group with group Scroll on
//! a canvas, marked on their bound layers with the group's window and Scroll, or with
//! why the group gets no span. Members are adjacent in item order (ADR-0182) and
//! nested scenes are refused (ADR-0188), so a group's drawn layers are one contiguous
//! range of the layer list.

use std::collections::BTreeMap;

use super::{BindProblem, BindReport, HoldScene, OutgoingScene, Role, State};
use crate::MAX_CANVAS_SIDE;
use crate::collection::SceneCollection;
use crate::flatten::Affine2;
use crate::model::{CanvasId, GroupId, GroupScroll, ItemContent, ItemId, Layout, SceneId, Scroll};

/// The limits of group span intermediates (ADR-0193 decision 4). The application sets
/// `max_side` from the device's `max_texture_dimension_2d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanLimits {
    /// Longest side of one intermediate, in pixels.
    pub max_side: u32,
    /// Most pixels of one intermediate.
    pub max_pixels: u64,
    /// Most spans of one compositor.
    pub max_spans: usize,
    /// Most pixels of one compositor's intermediates together.
    pub max_total_pixels: u64,
}

impl Default for SpanLimits {
    fn default() -> Self {
        Self {
            max_side: MAX_CANVAS_SIDE,
            max_pixels: 8_388_608,
            max_spans: 4,
            max_total_pixels: 16_777_216,
        }
    }
}

/// Why a group with group Scroll gets no span: its members are drawn transparent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SpanRefusal {
    /// The window is not finite or has no area.
    InvalidBounds,
    /// The window is over a per-intermediate limit.
    TooLarge,
    /// The compositor's span count or summed pixels were taken by groups below it.
    Capped,
}

/// A drawn member of a group with group Scroll on its canvas (ADR-0185).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroupMark {
    /// The canvas.
    pub canvas: CanvasId,
    /// The scene the group is in, as bound (a held copy's own id).
    pub scene: SceneId,
    /// The group.
    pub group: GroupId,
    /// The window in canvas pixels (x, y, width, height): the union of every member's
    /// picture box, hidden members included.
    pub window: [f32; 4],
    /// The saved settings, in layout units.
    pub settings: GroupScroll,
    /// The settings with speed, gap and fade in canvas pixels.
    pub scroll: Scroll,
    /// Set when the group has no span on this compositor.
    pub refused: Option<SpanRefusal>,
}

/// The scene each bound compositor draws.
fn shown(
    collection: &SceneCollection,
    (canvas, role): (CanvasId, Role),
    hold: &BTreeMap<CanvasId, HoldScene>,
    outgoing: &BTreeMap<CanvasId, OutgoingScene>,
) -> Option<SceneId> {
    let c = collection.canvas(canvas)?;
    match role {
        Role::Program => c.program,
        Role::Preview => c.preview,
        Role::Hold => hold.get(&canvas).map(|h| h.scene),
        Role::Incoming => outgoing
            .get(&canvas)
            .and_then(|o| o.incoming_scene)
            .or(c.program),
    }
}

/// Marks the group members of every bound compositor and reports groups without a
/// span. Without a group Scroll on a shown scene's canvas this only scans group lists.
pub(super) fn mark_groups(
    report: &mut BindReport,
    collection: &SceneCollection,
    state: &State,
    limits: SpanLimits,
    (hold, outgoing): (
        &BTreeMap<CanvasId, HoldScene>,
        &BTreeMap<CanvasId, OutgoingScene>,
    ),
) {
    let pictured = |item: &crate::model::SceneItem| match item.content {
        ItemContent::Source(s) => state
            .sources
            .get(&s)
            .is_some_and(|b| b.source.pins.output.is_some()),
        ItemContent::Scene(_) => false,
    };
    for (&key, bound) in &mut report.compositors {
        let Some(scene) = shown(collection, key, hold, outgoing) else {
            continue;
        };
        let canvas = key.0;
        let globals = collection.globals_of(canvas).filter(|g| *g != scene);
        let mut members: BTreeMap<ItemId, GroupMark> = BTreeMap::new();
        for s in [Some(scene), globals]
            .into_iter()
            .flatten()
            .filter_map(|s| collection.scene(s))
        {
            for g in &s.groups {
                let Some(&settings) = g.scrolls.get(&canvas) else {
                    continue;
                };
                let items = s.items.iter().filter(|i| i.group == Some(g.id));
                let boxes = items.clone().filter(|i| pictured(i));
                let Some((window, scale)) = window_of(collection, canvas, boxes) else {
                    continue;
                };
                let mark = GroupMark {
                    canvas,
                    scene: s.id,
                    group: g.id,
                    window,
                    settings,
                    scroll: settings.as_scroll().scaled(scale),
                    refused: None,
                };
                members.extend(items.map(|i| (i.id, mark)));
            }
        }
        if members.is_empty() {
            continue;
        }
        let (mut spans, mut total, mut last) = (0, 0u64, None);
        for layer in &mut bound.layers {
            let Some(mut mark) = layer.path.first().and_then(|i| members.get(i)).copied() else {
                continue;
            };
            let id = (mark.scene, mark.group);
            if last != Some(id) {
                last = Some(id);
                let refused = mark
                    .refused
                    .or_else(|| fits(mark.window, limits, spans, total));
                if refused.is_none() {
                    spans += 1;
                    total += texels(mark.window).map_or(0, |(w, h)| u64::from(w) * u64::from(h));
                }
                for m in members.values_mut().filter(|m| (m.scene, m.group) == id) {
                    m.refused = refused;
                }
                mark.refused = refused;
                if let Some(why) = refused {
                    let p = BindProblem::GroupSpanRefused(canvas, mark.scene, mark.group, why);
                    if !report.problems.contains(&p) {
                        report.problems.push(p);
                    }
                }
            }
            layer.group = Some(mark);
        }
    }
}

/// Why a span with `window` gets no intermediate, given `spans` spans of `total`
/// pixels below it.
fn fits(window: [f32; 4], limits: SpanLimits, spans: usize, total: u64) -> Option<SpanRefusal> {
    let Some((w, h)) = texels(window) else {
        return Some(SpanRefusal::InvalidBounds);
    };
    let px = u64::from(w) * u64::from(h);
    let side = limits.max_side.min(MAX_CANVAS_SIDE);
    if w > side || h > side || px > limits.max_pixels {
        Some(SpanRefusal::TooLarge)
    } else if spans >= limits.max_spans || total + px > limits.max_total_pixels {
        Some(SpanRefusal::Capped)
    } else {
        None
    }
}

/// The intermediate's size: the window's whole-pixel cover.
fn texels([x, y, w, h]: [f32; 4]) -> Option<(u32, u32)> {
    if !(w > 0.0 && h > 0.0 && x.is_finite() && y.is_finite() && (x + w + y + h).is_finite()) {
        return None;
    }
    let size = |a: f32, len: f32| ((a + len).ceil() - a.floor()).min(u32::MAX as f32) as u32;
    Some((size(x, w), size(y, h)))
}

/// The union of the members' picture boxes on `canvas` in canvas pixels, with the
/// layout-to-canvas scale; `None` without a member.
fn window_of<'a>(
    collection: &SceneCollection,
    canvas: CanvasId,
    members: impl Iterator<Item = &'a crate::model::SceneItem>,
) -> Option<([f32; 4], f32)> {
    let c = collection.canvas(canvas)?;
    let space = (c.layout_size.width, c.layout_size.height);
    let current = (c.size.width, c.size.height);
    let (mut lo, mut hi) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
    for item in members {
        let t = Affine2::of_layout(&item.layout_on(canvas).placed_in(space, current));
        for (u, v) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
            let (x, y) = t.apply(u, v);
            (lo, hi) = ([lo[0].min(x), lo[1].min(y)], [hi[0].max(x), hi[1].max(y)]);
        }
    }
    if lo[0] > hi[0] {
        return None;
    }
    let scale = Layout::new([0.0, 0.0], [1.0, 1.0])
        .placed_in(space, current)
        .size[0];
    Some(([lo[0], lo[1], hi[0] - lo[0], hi[1] - lo[1]], scale))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_refuse_oversized_then_cap_bottom_first() {
        let l = SpanLimits {
            max_side: 100,
            ..SpanLimits::default()
        };
        assert_eq!(
            fits([0.0, 0.0, 101.0, 10.0], l, 0, 0),
            Some(SpanRefusal::TooLarge)
        );
        assert_eq!(
            fits([0.5, 0.0, 99.6, 10.0], l, 0, 0),
            Some(SpanRefusal::TooLarge)
        );
        assert_eq!(
            fits([0.0, 0.0, 10.0, 10.0], l, 4, 0),
            Some(SpanRefusal::Capped)
        );
        assert_eq!(
            fits([0.0, 0.0, 10.0, 10.0], l, 1, l.max_total_pixels - 99),
            Some(SpanRefusal::Capped)
        );
        assert_eq!(
            fits([0.0, 0.0, f32::NAN, 1.0], l, 0, 0),
            Some(SpanRefusal::InvalidBounds)
        );
        assert_eq!(fits([0.0, 0.0, 10.0, 10.0], l, 3, 0), None);
    }
}
