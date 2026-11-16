//! A [`TextRenderer`] on `parley` and `swash` (ADR-0129 decision 2).
//!
//! `parley` lays out and shapes the text (line breaking, bidi, font fallback through
//! `fontique`, which enumerates the system's fonts); `swash` turns glyphs into coverage
//! masks, stroked masks for the outline, and colour bitmaps for emoji. The picture is
//! composed here on the CPU: background, blurred shadow, outline, fill, colour glyphs.
//!
//! Fonts and layout contexts are built lazily on the first call and dropped by
//! [`TextRenderer::trim`], so a registered renderer with no text source holds no font
//! memory (P13). Every call runs on the text worker, never on the render thread.

mod compose;

use std::borrow::Cow;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use parley::fontique::{Blob, Collection, CollectionOptions, SourceCache};
use parley::{
    Alignment, AlignmentOptions, FontContext, FontFamily, FontFamilyName, FontFeature,
    FontFeatures, FontStyle, FontWeight, GenericFamily, Layout, LayoutContext, LineHeight,
    PositionedLayoutItem, StyleProperty,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_text::{
    Align, FIT_MAX_STEPS, FIT_MIN_SCALE, Overflow, RenderNotes, TextBlock, TextError, TextPicture,
    TextRenderer, VAlign, caps,
};
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::zeno::{Format, Stroke, Vector};

use crate::compose::{Layer, Mask};

/// Where fonts come from.
#[derive(Clone)]
enum Fonts {
    /// The system's fonts; the default family is the system UI font.
    System,
    /// Only these font files; the default family is the first one's.
    Given(Vec<Arc<dyn AsRef<[u8]> + Send + Sync>>),
}

/// Fonts and contexts, built on first use.
struct State {
    fcx: FontContext,
    lcx: LayoutContext<()>,
    scx: ScaleContext,
    /// The default family for [`Fonts::Given`].
    default_family: Option<String>,
}

/// The parley renderer.
pub struct ParleyRenderer {
    caps: CapabilitySet,
    fonts: Fonts,
    state: Mutex<Option<State>>,
}

impl std::fmt::Debug for ParleyRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParleyRenderer").finish_non_exhaustive()
    }
}

impl ParleyRenderer {
    /// A renderer on the system's fonts.
    pub fn system() -> Self {
        Self::with(Fonts::System, true)
    }

    /// A renderer that knows only `fonts` (font file contents) and never reads the
    /// system's fonts: for tests and for fonts shipped with a document later.
    pub fn with_fonts(fonts: Vec<Vec<u8>>) -> Self {
        Self::with(
            Fonts::Given(
                fonts
                    .into_iter()
                    .map(|f| Arc::new(f) as Arc<dyn AsRef<[u8]> + Send + Sync>)
                    .collect(),
            ),
            false,
        )
    }

    fn with(fonts: Fonts, system: bool) -> Self {
        Self {
            caps: CapabilitySet::new()
                .with(caps::COLOR_GLYPHS, CapabilityValue::Flag(true))
                .with(caps::TABULAR_FIGURES, CapabilityValue::Flag(true))
                .with(caps::SYSTEM_FONTS, CapabilityValue::Flag(system)),
            fonts,
            state: Mutex::new(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<State>> {
        // A panic inside a render (caught by the worker) leaves the state usable: the
        // contexts hold caches only.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn build(&self) -> State {
        let (collection, default_family) = match &self.fonts {
            Fonts::System => (
                Collection::new(CollectionOptions {
                    shared: false,
                    system_fonts: true,
                }),
                None,
            ),
            Fonts::Given(files) => {
                let mut collection = Collection::new(CollectionOptions {
                    shared: false,
                    system_fonts: false,
                });
                let mut first = None;
                for data in files {
                    let added = collection.register_fonts(Blob::new(Arc::clone(data)), None);
                    if first.is_none() {
                        first = added.first().map(|(id, _)| *id);
                    }
                }
                let name = first.and_then(|id| collection.family_name(id).map(str::to_owned));
                (collection, name)
            }
        };
        State {
            fcx: FontContext {
                collection,
                source_cache: SourceCache::default(),
            },
            lcx: LayoutContext::new(),
            scx: ScaleContext::new(),
            default_family,
        }
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let mut guard = self.lock();
        let state = guard.get_or_insert_with(|| self.build());
        f(state)
    }
}

/// Space the outline and shadow take around the text: `[left, top, right, bottom]`.
fn extents(block: &TextBlock) -> [f32; 4] {
    let o = block.outline.map_or(0.0, |o| o.width);
    let mut e = [o; 4];
    if let Some(s) = block.shadow {
        let [dx, dy] = s.offset;
        e[0] = e[0].max(o + s.blur - dx);
        e[1] = e[1].max(o + s.blur - dy);
        e[2] = e[2].max(o + s.blur + dx);
        e[3] = e[3].max(o + s.blur + dy);
    }
    e.map(|v| v.max(0.0).ceil())
}

/// Lays out `block`'s text at `scale` of its font size, wrapped at `wrap`. Returns
/// the layout and whether the requested family was missing.
fn layout(
    state: &mut State,
    block: &TextBlock,
    scale: f32,
    wrap: Option<f32>,
) -> (Layout<()>, bool) {
    let mut missing = false;
    let family: FontFamily<'_> = match block.font.as_deref() {
        Some(name) if state.fcx.collection.family_id(name).is_some() => {
            FontFamily::Single(FontFamilyName::Named(Cow::Owned(name.to_owned())))
        }
        requested => {
            missing = requested.is_some();
            match &state.default_family {
                Some(name) => FontFamily::Single(FontFamilyName::Named(Cow::Owned(name.clone()))),
                None => FontFamily::Single(FontFamilyName::Generic(GenericFamily::SystemUi)),
            }
        }
    };
    let tnum = [FontFeature::new(parley::setting::Tag::new(b"tnum"), 1)];
    let mut builder = state
        .lcx
        .ranged_builder(&mut state.fcx, &block.text, 1.0, true);
    builder.push_default(StyleProperty::FontFamily(family));
    builder.push_default(StyleProperty::FontSize(block.size * scale));
    builder.push_default(StyleProperty::FontWeight(FontWeight::new(f32::from(
        block.weight,
    ))));
    if block.italic {
        builder.push_default(StyleProperty::FontStyle(FontStyle::Italic));
    }
    builder.push_default(StyleProperty::LineHeight(LineHeight::MetricsRelative(
        block.line_spacing,
    )));
    builder.push_default(StyleProperty::LetterSpacing(block.letter_spacing * scale));
    if block.tabular_figures {
        builder.push_default(StyleProperty::FontFeatures(FontFeatures::List(
            Cow::Borrowed(&tnum),
        )));
    }
    let mut layout = builder.build(&block.text);
    layout.break_all_lines(wrap);
    let alignment = match block.align {
        Align::Left => Alignment::Left,
        Align::Right => Alignment::Right,
        _ => Alignment::Center,
    };
    layout.align(
        alignment,
        AlignmentOptions {
            align_when_overflowing: true,
        },
    );
    (layout, missing)
}

/// Draws every glyph of `layout` at `origin` into `fill`, `stroke` (when an outline
/// is asked for) and the colour layers.
fn rasterize(
    state: &mut State,
    layout: &Layout<()>,
    origin: [f32; 2],
    outline: Option<f32>,
    fill: &mut Mask,
    mut stroke: Option<&mut Mask>,
    color: &mut Vec<Layer>,
) {
    for line in layout.lines() {
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };
            let run = glyph_run.run();
            let font = run.font();
            let Some(font_ref) =
                swash::FontRef::from_index(font.data.as_ref(), font.index as usize)
            else {
                continue;
            };
            let mut scaler = state
                .scx
                .builder(font_ref)
                .size(run.font_size())
                .hint(false)
                .normalized_coords(run.normalized_coords())
                .build();
            for glyph in glyph_run.positioned_glyphs() {
                let x = glyph.x + origin[0];
                let y = glyph.y + origin[1];
                let offset = Vector::new(x.fract(), y.fract());
                let Ok(id) = u16::try_from(glyph.id) else {
                    continue;
                };
                let (bx, by) = (x.floor() as i32, y.floor() as i32);
                if let Some(image) = Render::new(&[
                    Source::ColorOutline(0),
                    Source::ColorBitmap(StrikeWith::BestFit),
                    Source::Outline,
                ])
                .format(Format::Alpha)
                .offset(offset)
                .render(&mut scaler, id)
                {
                    let p = image.placement;
                    let at = [bx + p.left, by - p.top];
                    match image.content {
                        Content::Mask => fill.max(at, p.width, p.height, &image.data),
                        Content::Color => color.push(Layer {
                            at,
                            width: p.width,
                            height: p.height,
                            rgba: image.data,
                        }),
                        Content::SubpixelMask => {}
                    }
                }
                if let (Some(width), Some(stroke)) = (outline, stroke.as_deref_mut()) {
                    if let Some(image) = Render::new(&[Source::Outline])
                        .format(Format::Alpha)
                        .style(Stroke::new(width * 2.0))
                        .offset(offset)
                        .render(&mut scaler, id)
                    {
                        let p = image.placement;
                        stroke.max([bx + p.left, by - p.top], p.width, p.height, &image.data);
                    }
                }
            }
        }
    }
}

impl TextRenderer for ParleyRenderer {
    fn id(&self) -> &str {
        "parley"
    }

    fn capabilities(&self) -> &CapabilitySet {
        &self.caps
    }

    fn render(&self, block: &TextBlock) -> Result<(TextPicture, RenderNotes), TextError> {
        block.validate()?;
        let [bw, bh] = block.box_size;
        let ext = extents(block);
        let area_w = (bw as f32 - 2.0 * block.padding - ext[0] - ext[2]).max(1.0);
        let area_h = (bh as f32 - 2.0 * block.padding - ext[1] - ext[3]).max(1.0);
        let wrap = block.wrap.then_some(area_w);

        self.with_state(|state| {
            let fits = |state: &mut State, scale: f32| {
                let (l, _) = layout(state, block, scale, wrap);
                l.width() <= area_w + 0.5 && l.height() <= area_h + 0.5
            };
            let mut scale = 1.0;
            if block.overflow == Overflow::Fit && !block.text.is_empty() && !fits(state, 1.0) {
                let (mut lo, mut hi) = (FIT_MIN_SCALE, 1.0);
                for _ in 0..FIT_MAX_STEPS {
                    let mid = (lo + hi) / 2.0;
                    if fits(state, mid) {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                scale = lo;
            }
            let (layout, font_missing) = layout(state, block, scale, wrap);
            // Without wrapping, parley aligns lines within the longest one; place that
            // block inside the area by the same alignment.
            let shift_x = if wrap.is_some() {
                0.0
            } else {
                let free = area_w - layout.width();
                match block.align {
                    Align::Left => 0.0,
                    Align::Right => free,
                    _ => free / 2.0,
                }
            };
            let free_y = area_h - layout.height();
            let shift_y = match block.valign {
                VAlign::Top => 0.0,
                VAlign::Bottom => free_y,
                _ => free_y / 2.0,
            };
            let origin = [
                block.padding + ext[0] + shift_x,
                block.padding + ext[1] + shift_y,
            ];

            let mut fill = Mask::new(bw, bh);
            let mut stroke = block.outline.map(|_| Mask::new(bw, bh));
            let mut color = Vec::new();
            rasterize(
                state,
                &layout,
                origin,
                block.outline.map(|o| o.width),
                &mut fill,
                stroke.as_mut(),
                &mut color,
            );
            let rgba = compose::compose(block, &fill, stroke.as_ref(), &color);
            let picture = TextPicture::new(bw, bh, rgba)
                .ok_or_else(|| TextError::Render("picture size".to_owned()))?;
            let mut notes = RenderNotes::default();
            notes.font_missing = font_missing;
            Ok((picture, notes))
        })
    }

    fn measure(&self, block: &TextBlock, max_width: Option<u32>) -> Result<[u32; 2], TextError> {
        let mut probe = block.clone();
        probe.box_size = [1, 1];
        probe.validate()?;
        let ext = extents(block);
        self.with_state(|state| {
            let (layout, _) = layout(state, block, 1.0, max_width.map(|w| w as f32));
            let pad = 2.0 * block.padding;
            Ok([
                (layout.width() + pad + ext[0] + ext[2]).ceil() as u32,
                (layout.height() + pad + ext[1] + ext[3]).ceil() as u32,
            ])
        })
    }

    fn families(&self) -> Result<Vec<String>, TextError> {
        self.with_state(|state| {
            let mut names: Vec<String> = state
                .fcx
                .collection
                .family_names()
                .map(str::to_owned)
                .collect();
            names.sort_by_key(|n| n.to_lowercase());
            names.dedup();
            Ok(names)
        })
    }

    fn trim(&self) {
        *self.lock() = None;
    }
}
