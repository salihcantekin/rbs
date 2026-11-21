//! The pointer host service for filter types (ADR-0165 decision 3; spec
//! `video-filters.md`, "Host service: pointer").
//!
//! A type that declares [`Needs::POINTER`] gets, every render tick while Active, a
//! [`PointerView`] of the pointer on its input picture and of the recent clicks over
//! it, and returns its live [`Effects`], which the node uploads to bind group 2. The
//! node owns the input subscription (opened in `prepare`, closed in `release`, both on
//! the control thread) and the click ring; nothing here allocates per tick.

use std::fmt;

use rbs_capture::{Cover, MappingCell, PictureMapping};
use rbs_core::time::Timestamp;
use rbs_input::{Button, EventMask, InputEvent, InputMonitor, InputReader, Subscription};

use crate::types::{FilterType, Values};

/// Host services a filter type uses ([`crate::FilterType::needs`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Needs(u8);

impl Needs {
    /// No host service.
    pub const NONE: Needs = Needs(0);
    /// The pointer view and the effect buffer.
    pub const POINTER: Needs = Needs(1);

    /// Whether every service in `other` is in `self`.
    pub const fn contains(self, other: Needs) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Live effects one tick may draw.
pub const MAX_EFFECTS: usize = 16;
/// Clicks the node keeps for its type, newest last.
pub const MAX_CLICKS: usize = 16;
/// `vec4<f32>` entries of the effect buffer: a header and two per effect.
pub(crate) const EFFECT_VECS: usize = 1 + 2 * MAX_EFFECTS;
/// The pointer prelude, appended to the shader prelude for pointer types.
pub const POINTER_PRELUDE: &str = include_str!("shaders/pointer_prelude.wgsl");

/// A button-down over the picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Click {
    /// When the button went down (master time).
    pub time: Timestamp,
    /// Where, in pixels of the input picture.
    pub at: [f32; 2],
    /// Which button.
    pub button: Button,
}

/// What a pointer type sees on one tick.
#[derive(Debug, Clone, Copy)]
pub struct PointerView<'a> {
    /// The tick's time; effects age on it.
    pub now: Timestamp,
    /// The input frame's timestamp.
    pub frame_time: Timestamp,
    /// The input picture's size in pixels.
    pub size: [u32; 2],
    /// The pointer in picture pixels at `frame_time`; `None` when unknown, outside
    /// the picture or over a window covering the captured one.
    pub pointer: Option<[f32; 2]>,
    /// Button-downs over the picture, oldest first, at most [`MAX_CLICKS`].
    pub clicks: &'a [Click],
}

/// One live effect: two `vec4<f32>` whose meaning the type's shader decides.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Effect(pub [[f32; 4]; 2]);

/// The live effects of one tick, at most [`MAX_EFFECTS`].
#[derive(Clone)]
pub struct Effects {
    len: usize,
    items: [Effect; MAX_EFFECTS],
}

impl Default for Effects {
    fn default() -> Self {
        Self {
            len: 0,
            items: [Effect::default(); MAX_EFFECTS],
        }
    }
}

impl fmt::Debug for Effects {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl Effects {
    /// Adds an effect; `false` (and nothing added) when [`MAX_EFFECTS`] are live.
    pub fn push(&mut self, effect: Effect) -> bool {
        let Some(slot) = self.items.get_mut(self.len) else {
            return false;
        };
        *slot = effect;
        self.len += 1;
        true
    }

    /// The live effects.
    pub fn as_slice(&self) -> &[Effect] {
        &self.items[..self.len]
    }

    /// How many are live.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether none is live.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Removes them all.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// The effect buffer's contents: the count in the header, then the effects.
    pub(crate) fn write_to(&self, out: &mut [[f32; 4]; EFFECT_VECS]) {
        #[allow(clippy::cast_precision_loss)] // At most MAX_EFFECTS.
        let count = self.len as f32;
        out[0] = [count, 0.0, 0.0, 0.0];
        for (i, effect) in self.as_slice().iter().enumerate() {
            out[1 + 2 * i] = effect.0[0];
            out[2 + 2 * i] = effect.0[1];
        }
    }
}

/// What the host lends a pointer type's node: the shared input monitor and the
/// source's mapping cell (capture-port "Picture mapping" rule 6).
#[derive(Clone)]
pub struct PointerFeed {
    /// The process's input monitor; the node subscribes in `prepare`.
    pub monitor: InputMonitor,
    /// The newest frame's desktop to picture mapping of the filtered source.
    pub cell: MappingCell,
}

impl fmt::Debug for PointerFeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PointerFeed")
            .field("backend", &self.monitor.backend_name())
            .finish_non_exhaustive()
    }
}

impl PointerFeed {
    /// A feed of `monitor` and `cell`.
    pub fn new(monitor: InputMonitor, cell: MappingCell) -> Self {
        Self { monitor, cell }
    }
}

/// The node side: subscription, mapping and click ring.
pub(crate) struct PointerState {
    feed: Option<PointerFeed>,
    /// Open between `prepare` and `release`.
    live: Option<(Subscription, InputReader)>,
    mapping: Option<PictureMapping>,
    clicks: [Option<Click>; MAX_CLICKS],
    /// Oldest first, `clicks[..count]` after `ordered` copies them.
    ordered: [Click; MAX_CLICKS],
    next: usize,
    effects: Effects,
    /// The effect buffer's contents, packed from `effects`.
    packed: [[f32; 4]; EFFECT_VECS],
    /// The last tick drew effects, so idle must publish the input once more.
    pub(crate) drew: bool,
}

const NO_CLICK: Click = Click {
    time: Timestamp::ZERO,
    at: [0.0; 2],
    button: Button::Left,
};

impl Default for Click {
    fn default() -> Self {
        NO_CLICK
    }
}

impl Default for PointerState {
    fn default() -> Self {
        Self {
            feed: None,
            live: None,
            mapping: None,
            clicks: [None; MAX_CLICKS],
            ordered: [Click::default(); MAX_CLICKS],
            next: 0,
            effects: Effects::default(),
            packed: [[0.0; 4]; EFFECT_VECS],
            drew: false,
        }
    }
}

impl PointerState {
    pub(crate) fn with_feed(feed: PointerFeed) -> Self {
        Self {
            feed: Some(feed),
            ..Self::default()
        }
    }

    /// Control thread (`prepare`): opens the subscription. `Err` when it failed; the
    /// node then runs without input.
    pub(crate) fn open(&mut self) -> Result<(), ()> {
        self.close();
        let Some(feed) = &self.feed else {
            return Ok(());
        };
        let live = feed
            .monitor
            .subscribe(EventMask::POINTER.union(EventMask::BUTTONS))
            .map_err(|_| ())?;
        self.live = Some(live);
        Ok(())
    }

    /// Control thread (`release`): closes the subscription and forgets the clicks.
    pub(crate) fn close(&mut self) {
        self.live = None;
        self.mapping = None;
        self.clicks = [None; MAX_CLICKS];
        self.next = 0;
        self.effects.clear();
        self.drew = false;
    }

    /// Render thread, every tick: the type's live effects for this tick, packed for the
    /// effect buffer. `true` when at least one is live (and `drew` is then set).
    pub(crate) fn step(
        &mut self,
        ty: &dyn FilterType,
        values: &Values,
        now: Timestamp,
        frame_time: Timestamp,
        size: [u32; 2],
    ) -> bool {
        // A move of a fixed-size array, no allocation; lets the view borrow `self`.
        let mut effects = std::mem::take(&mut self.effects);
        effects.clear();
        if let Some(view) = self.view(now, frame_time, size) {
            ty.effects(values, &view, &mut effects);
        }
        effects.write_to(&mut self.packed);
        self.effects = effects;
        let live = !self.effects.is_empty();
        self.drew |= live;
        live
    }

    /// The effect buffer's contents from the last [`Self::step`].
    pub(crate) fn packed(&self) -> &[[f32; 4]; EFFECT_VECS] {
        &self.packed
    }

    /// Render thread, every tick: drains events and fills the view. Returns `None`
    /// without input or before the first mapping.
    pub(crate) fn view(
        &mut self,
        now: Timestamp,
        frame_time: Timestamp,
        size: [u32; 2],
    ) -> Option<PointerView<'_>> {
        let feed = self.feed.as_ref()?;
        let (_, reader) = self.live.as_mut()?;
        // A `None` read lost to a publish: keep the last mapping (rule 6).
        if let Some(m) = feed.cell.get() {
            self.mapping = Some(m);
        }
        let mapping = self.mapping?;
        let covered = mapping.cover == Cover::Covered;
        let to_picture = |x: f64, y: f64| -> Option<[f32; 2]> {
            let at = [x, y];
            if covered || !mapping.contains(at) {
                return None;
            }
            let [px, py] = mapping.to_picture(at);
            #[allow(clippy::cast_possible_truncation)] // Picture pixels fit f32.
            Some([px as f32, py as f32])
        };
        while let Some(timed) = reader.next_event() {
            if let InputEvent::ButtonDown { button, at } = timed.event
                && let Some(at) = to_picture(at.x, at.y)
            {
                self.clicks[self.next] = Some(Click {
                    time: timed.time,
                    at,
                    button,
                });
                self.next = (self.next + 1) % MAX_CLICKS;
            }
        }
        let pointer = reader
            .pointer_at(frame_time)
            .or_else(|| reader.latest_pointer())
            .and_then(|s| to_picture(s.at.x, s.at.y));
        let mut count = 0;
        for i in 0..MAX_CLICKS {
            if let Some(click) = self.clicks[(self.next + i) % MAX_CLICKS] {
                self.ordered[count] = click;
                count += 1;
            }
        }
        Some(PointerView {
            now,
            frame_time,
            size,
            pointer,
            clicks: &self.ordered[..count],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effects_hold_at_most_sixteen_and_pack_their_count() {
        let mut fx = Effects::default();
        for i in 0..MAX_EFFECTS {
            #[allow(clippy::cast_precision_loss)]
            let v = i as f32;
            assert!(fx.push(Effect([[v; 4], [v + 0.5; 4]])));
        }
        assert!(!fx.push(Effect::default()), "the seventeenth is refused");
        let mut out = [[0.0; 4]; EFFECT_VECS];
        fx.write_to(&mut out);
        assert_eq!(out[0][0], 16.0);
        assert_eq!(out[1], [0.0; 4]);
        assert_eq!(out[2], [0.5; 4]);
        assert_eq!(out[32], [15.5; 4]);
        fx.clear();
        assert!(fx.is_empty());
    }

    #[test]
    fn needs_contains() {
        assert!(Needs::POINTER.contains(Needs::POINTER));
        assert!(Needs::POINTER.contains(Needs::NONE));
        assert!(!Needs::NONE.contains(Needs::POINTER));
    }
}
