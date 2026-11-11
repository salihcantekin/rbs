//! Turning a scene into the flat layer list a compositor draws, and finding the
//! sources a collection needs running.

use std::collections::BTreeSet;

use crate::collection::{SceneCollection, SceneError, allowed_on};
use crate::model::{CanvasId, Crop, ItemContent, ItemId, ItemStyle, Layout, SceneId, SourceId};

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
    /// space is scaled to the box.
    pub fn flatten(&self, canvas: CanvasId, scene: SceneId) -> Result<Vec<FlatLayer>, SceneError> {
        let c = self
            .canvas(canvas)
            .ok_or(SceneError::UnknownCanvas(canvas))?;
        let s = self.scene(scene).ok_or(SceneError::UnknownScene(scene))?;
        if !allowed_on(s.scope, canvas) {
            return Err(SceneError::ScopeViolation(scene));
        }
        let size = (c.size.width as f32, c.size.height as f32);
        let mut out = Vec::new();
        let mut path = Vec::new();
        self.flatten_into(
            canvas,
            scene,
            size,
            Affine2::IDENTITY,
            1.0,
            &mut path,
            &mut out,
        );
        // Global sources (ADR-0065) are drawn above every scene the canvas shows.
        if let Some(globals) = c.globals.filter(|g| *g != scene) {
            self.flatten_into(
                canvas,
                globals,
                size,
                Affine2::IDENTITY,
                1.0,
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
        size: (f32, f32),
        outer: Affine2,
        opacity: f32,
        path: &mut Vec<ItemId>,
        out: &mut Vec<FlatLayer>,
    ) {
        // Validation guarantees the scene exists and nesting is acyclic.
        let Some(s) = self.scene(scene) else {
            return;
        };
        for item in s.items.iter().filter(|i| i.visible) {
            let layout = item.layout_on(canvas);
            let alpha = opacity * layout.opacity;
            if alpha <= 0.0 {
                continue;
            }
            let placed = Affine2::of_layout(layout).then(&outer);
            path.push(item.id);
            match item.content {
                ItemContent::Source(source) => out.push(FlatLayer {
                    source,
                    path: path.clone(),
                    transform: placed,
                    crop: layout.crop,
                    opacity: alpha,
                    style: layout.style,
                    box_size: [layout.size[0].abs(), layout.size[1].abs()],
                }),
                ItemContent::Scene(nested) => {
                    let inner = Affine2::scale(1.0 / size.0, 1.0 / size.1).then(&placed);
                    self.flatten_into(canvas, nested, size, inner, alpha, path, out);
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
                match item.content {
                    ItemContent::Source(source) => {
                        // A deactivated source is not drawn and is not wanted.
                        // A hidden one is not drawn either; `wanted_sources` keeps it
                        // running so showing it again does not wait for the device.
                        let standby = self.source(source).is_some_and(|src| src.standby());
                        if item.visible && !standby {
                            found.insert(source);
                            shown.insert(source);
                        }
                    }
                    ItemContent::Scene(nested) if item.visible => stack.push(nested),
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

    /// Sources the scenes need running: those placed in the program or preview scene
    /// of any canvas and not deactivated (media graph spec, Lifecycle). A hidden item
    /// still counts, so showing it does not wait for the device. Projectors, outputs
    /// and audio-only use add to this in the graph binder; a source wanted by none of
    /// them is Standby.
    pub fn wanted_sources(&self) -> BTreeSet<SourceId> {
        let mut wanted = BTreeSet::new();
        for c in self.canvases() {
            for scene in [c.program, c.preview].into_iter().flatten() {
                if let Ok(sources) = self.visible_sources(c.id, scene) {
                    wanted.extend(sources);
                }
                wanted.extend(self.hidden_kept_warm(c.id, scene));
            }
        }
        wanted
    }

    /// Hidden sources in `scene` that stay open: not drawn, not inside a hidden
    /// nested scene, and not deactivated. Showing one does not wait for the device.
    fn hidden_kept_warm(&self, canvas: CanvasId, scene: SceneId) -> BTreeSet<SourceId> {
        let mut warm = BTreeSet::new();
        let Some(c) = self.canvas(canvas) else {
            return warm;
        };
        let Some(s) = self.scene(scene) else {
            return warm;
        };
        if !allowed_on(s.scope, c.id) {
            return warm;
        }
        let mut seen = BTreeSet::new();
        let mut stack: Vec<SceneId> = c.globals.into_iter().chain([scene]).collect();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let Some(s) = self.scene(id) else {
                continue;
            };
            for item in &s.items {
                match item.content {
                    ItemContent::Source(source) if !item.visible => {
                        let standby = self.source(source).is_some_and(|src| src.standby());
                        if !standby {
                            warm.insert(source);
                        }
                    }
                    ItemContent::Scene(nested) if item.visible => stack.push(nested),
                    _ => {}
                }
            }
        }
        warm
    }
}
