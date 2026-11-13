//! Turning a scene into the flat layer list a compositor draws, and finding the
//! sources a collection needs running.

use std::collections::BTreeSet;

use crate::collection::{SceneCollection, SceneError, allowed_on};
use crate::model::{
    CanvasId, Crop, ItemContent, ItemId, ItemStyle, Layout, SceneId, SceneItem, SourceId,
};

/// Media file source. Temporal: Active only while a shown scene draws it (ADR-0107).
const TEMPORAL_MEDIA: &str = "org.rbs.source.media";
/// Browser source. Temporal, same rule as [`TEMPORAL_MEDIA`] (ADR-0107).
const TEMPORAL_BROWSER: &str = "org.rbs.source.browser";

/// A 2D affine transform, with the same convention as `rbs-gpu`'s layer transform:
/// maps `(x, y)` to `(a*x + c*y + tx, b*x + d*y + ty)`; for layers it maps the unit
/// square of the (cropped) source to canvas pixels, origin top left, y down.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine2 {
    /// Column 0, x component.
    pub a: f32,
    /// Column 0, y component.
    pub b: f32,
    /// Column 1, x component.
    pub c: f32,
    /// Column 1, y component.
    pub d: f32,
    /// Translation x.
    pub tx: f32,
    /// Translation y.
    pub ty: f32,
}

impl Affine2 {
    /// The identity.
    pub const IDENTITY: Self = Self::new(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);

    /// A transform from its six coefficients.
    pub const fn new(a: f32, b: f32, c: f32, d: f32, tx: f32, ty: f32) -> Self {
        Self { a, b, c, d, tx, ty }
    }

    fn scale(x: f32, y: f32) -> Self {
        Self::new(x, 0.0, 0.0, y, 0.0, 0.0)
    }

    fn translation(x: f32, y: f32) -> Self {
        Self::new(1.0, 0.0, 0.0, 1.0, x, y)
    }

    fn rotation_degrees(degrees: f32) -> Self {
        let (s, c) = degrees.to_radians().sin_cos();
        Self::new(c, s, -s, c, 0.0, 0.0)
    }

    /// The transform that applies `self` first, then `next`.
    pub fn then(&self, next: &Self) -> Self {
        Self {
            a: next.a * self.a + next.c * self.b,
            b: next.b * self.a + next.d * self.b,
            c: next.a * self.c + next.c * self.d,
            d: next.b * self.c + next.d * self.d,
            tx: next.a * self.tx + next.c * self.ty + next.tx,
            ty: next.b * self.tx + next.d * self.ty + next.ty,
        }
    }

    /// Applies the transform to a point.
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.tx,
            self.b * x + self.d * y + self.ty,
        )
    }

    /// The transform that maps the unit square to the box of `layout`: scaled to its
    /// size, rotated about the box centre, moved to its position.
    pub fn of_layout(layout: &Layout) -> Self {
        let [x, y] = layout.position;
        let [w, h] = layout.size;
        Self::scale(w, h)
            .then(&Self::translation(-w / 2.0, -h / 2.0))
            .then(&Self::rotation_degrees(layout.rotation))
            .then(&Self::translation(x + w / 2.0, y + h / 2.0))
    }
}

/// One layer for the compositor: a source drawn with a transform, crop and opacity.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct FlatLayer {
    /// The source drawn.
    pub source: SourceId,
    /// Items from the shown scene down to this source's item (nested scenes add one
    /// entry each); the last entry is the source item. For selection and hit testing.
    pub path: Vec<ItemId>,
    /// Maps the unit square of the cropped source to canvas pixels.
    pub transform: Affine2,
    /// Source pixels cut before drawing; the compositor turns this into texture
    /// coordinates with the source's actual size.
    pub crop: Crop,
    /// Product of the opacities along the path.
    pub opacity: f32,
    /// Shape, border and shadow, with lengths in the item's own box units (ADR-0053):
    /// the compositor evaluates them on the unit square scaled by `box_size`, so they
    /// transform with the picture, nested scenes included.
    pub style: ItemStyle,
    /// The item's own box size (`|Layout::size|`), the units of `style`.
    pub box_size: [f32; 2],
}

impl SceneCollection {
    /// The layers that draw `scene` on `canvas`, bottom first, with nested scenes
    /// expanded and per-canvas layouts applied (U15), then the layers of the canvas's
    /// global scene (ADR-0065), whose paths start with the global item. Hidden items, and everything
    /// inside a hidden nested scene, are left out; so are fully transparent ones.
    ///
    /// A nested scene is drawn into its item's box: its own canvas-sized coordinate
    /// space is scaled to the box. This is [`Self::flatten_cued`] with an empty cue set.
    pub fn flatten(&self, canvas: CanvasId, scene: SceneId) -> Result<Vec<FlatLayer>, SceneError> {
        self.flatten_cued(canvas, scene, &BTreeSet::new())
    }

    /// Like [`Self::flatten`], and also draws an item whose id is in `cued` even when
    /// `visible` is false (ADR-0105). The set is not written onto the item.
    ///
    /// An empty set does not look up any id. Nested content under a hidden nested
    /// scene stays omitted unless that nested-scene item is itself visible or cued.
    /// Callers pass cues only for a shown scene (the canvas program, or preview when
    /// set): an item that is not in that scene's tree, including a cue on another
    /// scene, is not in the result.
    ///
    /// Hot path (ADR-0105 decision 8): this merge is a lookup of the active cue map,
    /// bounded by running cues, on the control thread when the layer list is rebuilt.
    /// It is not a per-frame scan of every item, and it does not run from
    /// `Node::process`. The implementation PR records `hot-path-review` for the merge.
    pub fn flatten_cued(
        &self,
        canvas: CanvasId,
        scene: SceneId,
        cued: &BTreeSet<ItemId>,
    ) -> Result<Vec<FlatLayer>, SceneError> {
        let c = self
            .canvas(canvas)
            .ok_or(SceneError::UnknownCanvas(canvas))?;
        let s = self.scene(scene).ok_or(SceneError::UnknownScene(scene))?;
        if !allowed_on(s.scope, canvas) {
            return Err(SceneError::ScopeViolation(scene));
        }
        let space = (c.layout_size.width, c.layout_size.height);
        let current = (c.size.width, c.size.height);
        let size = (c.size.width as f32, c.size.height as f32);
        let mut out = Vec::new();
        let mut path = Vec::new();
        // Empty cue map: the loop does not probe a set (P13).
        let cued = (!cued.is_empty()).then_some(cued);
        self.flatten_into(
            canvas,
            scene,
            space,
            current,
            size,
            Affine2::IDENTITY,
            1.0,
            cued,
            &mut path,
            &mut out,
        );
        // Global sources (ADR-0065) are drawn above every scene the canvas shows.
        if let Some(globals) = c.globals.filter(|g| *g != scene) {
            self.flatten_into(
                canvas,
                globals,
                space,
                current,
                size,
                Affine2::IDENTITY,
                1.0,
                cued,
                &mut path,
                &mut out,
            );
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn flatten_into(
        &self,
        canvas: CanvasId,
        scene: SceneId,
        space: (u32, u32),
        current: (u32, u32),
        size: (f32, f32),
        outer: Affine2,
        opacity: f32,
        cued: Option<&BTreeSet<ItemId>>,
        path: &mut Vec<ItemId>,
        out: &mut Vec<FlatLayer>,
    ) {
        // Validation guarantees the scene exists and nesting is acyclic.
        let Some(s) = self.scene(scene) else {
            return;
        };
        for item in s.items.iter().filter(|i| item_drawn(i, cued)) {
            let layout = item.layout_on(canvas).placed_in(space, current);
            let alpha = opacity * layout.opacity;
            if alpha <= 0.0 {
                continue;
            }
            let placed = Affine2::of_layout(&layout).then(&outer);
            path.push(item.id);
            match item.content {
                ItemContent::Source(source) => {
                    // Region and item crop are both full-picture insets. One crop
                    // goes to the compositor; it turns that into one UV rect. An
                    // empty intersection with the delivered picture is the existing
                    // empty-crop skip (ADR-0103). This is a pre-frame edit of the
                    // crop field, not a per-layer branch in the compositor.
                    let crop = self
                        .source(source)
                        .and_then(|s| s.region)
                        .map(|region| region.intersect_crop(layout.crop))
                        .unwrap_or(layout.crop);
                    out.push(FlatLayer {
                        source,
                        path: path.clone(),
                        transform: placed,
                        crop,
                        opacity: alpha,
                        style: layout.style,
                        box_size: [layout.size[0].abs(), layout.size[1].abs()],
                    });
                }
                ItemContent::Scene(nested) => {
                    let inner = Affine2::scale(1.0 / size.0, 1.0 / size.1).then(&placed);
                    self.flatten_into(
                        canvas, nested, space, current, size, inner, alpha, cued, path, out,
                    );
                }
            }
            path.pop();
        }
    }

    /// Sources drawn in `scene` on `canvas`, including nested scenes and the
    /// canvas's global scene (ADR-0065): every visible source item that is not
    /// inside a hidden nested scene and that the user has not deactivated, whatever
    /// its opacity (a fade from 0, or a source used for its audio, still needs it
    /// linked). A hidden source is not drawn; [`Self::wanted_sources`] keeps it
    /// running so showing it is immediate. A companion is heard only while its
    /// picture is drawn.
    pub fn visible_sources(
        &self,
        canvas: CanvasId,
        scene: SceneId,
    ) -> Result<BTreeSet<SourceId>, SceneError> {
        self.visible_sources_cued(canvas, scene, &BTreeSet::new())
    }

    /// Like [`Self::visible_sources`], and a cued hidden item counts as drawn for
    /// video and for the program mixer (ADR-0105 decision 6). An empty set does not
    /// look up any id. A hidden nested scene is not entered unless that nested item
    /// is visible or cued.
    pub fn visible_sources_cued(
        &self,
        canvas: CanvasId,
        scene: SceneId,
        cued: &BTreeSet<ItemId>,
    ) -> Result<BTreeSet<SourceId>, SceneError> {
        let c = self
            .canvas(canvas)
            .ok_or(SceneError::UnknownCanvas(canvas))?;
        let s = self.scene(scene).ok_or(SceneError::UnknownScene(scene))?;
        if !allowed_on(s.scope, c.id) {
            return Err(SceneError::ScopeViolation(scene));
        }
        let mut found = BTreeSet::new();
        let mut shown = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let cued = (!cued.is_empty()).then_some(cued);
        // Global sources (ADR-0065) are part of every scene the canvas shows.
        let mut stack: Vec<SceneId> = c.globals.into_iter().chain([scene]).collect();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let Some(s) = self.scene(id) else {
                continue;
            };
            for item in &s.items {
                let show = item_drawn(item, cued);
                match item.content {
                    ItemContent::Source(source) => {
                        // A deactivated source is not drawn and is not wanted.
                        // A hidden one is not drawn either, unless a timed-show cue
                        // names it (ADR-0105); `wanted_sources` keeps capture and
                        // stills running so showing them again does not wait.
                        let standby = self.source(source).is_some_and(|src| src.standby());
                        if show && !standby {
                            found.insert(source);
                            shown.insert(source);
                        }
                    }
                    ItemContent::Scene(nested) if show => stack.push(nested),
                    ItemContent::Scene(_) => {}
                }
            }
        }
        // A "Capture audio too" companion is audible only while its picture is
        // drawn (ADR-0096): sound is never on air without its picture. A hidden
        // picture stays open, but its companion is not heard until the picture shows.
        found.retain(|id| {
            self.source(*id)
                .and_then(|s| s.companion_of())
                .is_none_or(|picture| shown.contains(&picture))
        });
        Ok(found)
    }

    /// Sources the collection needs running: every source placed in any scene, unless
    /// the user deactivated it (`source.set_active`, the standby hint). Hiding an item
    /// and leaving the program or preview scene do not release the device, so showing
    /// the source again, or switching back to its scene, does not wait for the camera
    /// to open. A source that is not placed anywhere stays Standby.
    ///
    /// `org.rbs.source.media` and `org.rbs.source.browser` are still listed here when
    /// placed. [`Self::running_sources`] is the set the binder sets Active.
    pub fn wanted_sources(&self) -> BTreeSet<SourceId> {
        let mut wanted = BTreeSet::new();
        for scene in self.scenes() {
            for item in &scene.items {
                if let ItemContent::Source(id) = item.content
                    && self.source(id).is_some_and(|source| !source.standby())
                {
                    wanted.insert(id);
                }
            }
        }
        wanted
    }

    /// Sources the binder sets Active (ADR-0107).
    ///
    /// Capture sources and stills are [`Self::wanted_sources`]: placed and not
    /// deactivated, even when hidden or off the shown scene. `org.rbs.source.media`
    /// and `org.rbs.source.browser` are Active only when some canvas's program scene,
    /// or its preview scene when that field is set, draws them with `visible` true.
    /// Opacity does not matter, including 0. A hidden item does not, unless the host
    /// passes its source in [`Self::running_sources_cued`]. A scene that is
    /// not program or preview does not. One visible placement across the canvases is
    /// enough. A follower counts once the host has written the scene it is showing,
    /// including a followed program, into `program`.
    pub fn running_sources(&self) -> BTreeSet<SourceId> {
        self.running_sources_cued(&BTreeSet::new())
    }

    /// Like [`Self::running_sources`], and temporal sources in `cued` are Active too
    /// (ADR-0105 decision 7, ADR-0107 decision 4). The host passes source ids of
    /// items a shown scene is actually drawing for a cue. An empty set adds nothing.
    /// Capture sources and stills stay on [`Self::wanted_sources`]. A deactivated
    /// source is not made Active.
    pub fn running_sources_cued(&self, cued: &BTreeSet<SourceId>) -> BTreeSet<SourceId> {
        let mut running = BTreeSet::new();
        for id in self.wanted_sources() {
            if !self.is_temporal(id) {
                running.insert(id);
            }
        }
        for canvas in self.canvases() {
            for scene in [canvas.program, canvas.preview].into_iter().flatten() {
                let Ok(visible) = self.visible_sources(canvas.id, scene) else {
                    continue;
                };
                for id in visible {
                    if self.is_temporal(id) {
                        running.insert(id);
                    }
                }
            }
        }
        if !cued.is_empty() {
            for id in cued {
                if self.is_temporal(*id) && self.source(*id).is_some_and(|source| !source.standby())
                {
                    running.insert(*id);
                }
            }
        }
        running
    }

    /// Source ids that a shown scene draws only because `cued` names the item
    /// (ADR-0105). Empty `cued` returns an empty set and does not walk scenes.
    /// The host passes this set into [`Self::running_sources_cued`] and into
    /// `SceneBinder::sync_cued`.
    pub fn cued_drawn_sources(&self, cued: &BTreeSet<ItemId>) -> BTreeSet<SourceId> {
        if cued.is_empty() {
            return BTreeSet::new();
        }
        let mut out = BTreeSet::new();
        for canvas in self.canvases() {
            for scene in [canvas.program, canvas.preview].into_iter().flatten() {
                let Ok(with) = self.visible_sources_cued(canvas.id, scene, cued) else {
                    continue;
                };
                let Ok(plain) = self.visible_sources(canvas.id, scene) else {
                    continue;
                };
                out.extend(with.difference(&plain).copied());
            }
        }
        out
    }

    /// True for the two temporal type ids (ADR-0107). No other type, including a
    /// plugin, is temporal.
    fn is_temporal(&self, id: SourceId) -> bool {
        self.source(id).is_some_and(|source| {
            matches!(source.type_id.as_str(), TEMPORAL_MEDIA | TEMPORAL_BROWSER)
        })
    }

    /// Wanted sources placed on a scene `canvas` may show (including its global scene).
    /// The program compositor keeps these linked so `plan()` does not drop them for
    /// having no sink while they are hidden or off the current scene; layers still come
    /// only from [`Self::flatten`].
    ///
    /// Temporal sources are omitted. A hidden temporal source is not linked. One a
    /// shown scene draws is linked from that scene's `visible_sources`, not from here.
    /// For a canvas that follows another, only scenes the leader can show count.
    pub fn warm_sources(&self, canvas: CanvasId) -> BTreeSet<SourceId> {
        let Some(c) = self.canvas(canvas) else {
            return BTreeSet::new();
        };
        let globals = c.globals;
        let mut showable: BTreeSet<SceneId> = self
            .showable_scenes(canvas)
            .into_iter()
            .map(|s| s.id)
            .collect();
        // A following canvas shows only scenes its leader can put on air (ADR-0093
        // decision 3), so its own scenes do not keep their devices open (P13).
        if let Some(leader) = c.follow.filter(|l| *l != canvas) {
            let led: BTreeSet<SceneId> = self
                .showable_scenes(leader)
                .into_iter()
                .map(|s| s.id)
                .collect();
            showable.retain(|s| led.contains(s));
        }
        self.wanted_sources()
            .into_iter()
            .filter(|id| !self.is_temporal(*id))
            .filter(|id| {
                self.placements_of(*id)
                    .iter()
                    .any(|(sc, _)| showable.contains(sc) || globals == Some(*sc))
            })
            .collect()
    }
}

/// Visible, or named by a non-empty cue set. An empty cue (`None`) does not look
/// the id up.
fn item_drawn(item: &SceneItem, cued: Option<&BTreeSet<ItemId>>) -> bool {
    item.visible || cued.is_some_and(|ids| ids.contains(&item.id))
}
