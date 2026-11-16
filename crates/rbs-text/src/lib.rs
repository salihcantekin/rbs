//! Text rendering port (ADR-0129, spec `docs/specs/text-port.md`).
//!
//! A [`TextRenderer`] lays out a [`TextBlock`] and draws it into a [`TextPicture`] of
//! exactly the block's box size: straight-alpha sRGB `Rgba8`. Renderers run off the
//! render thread (the text worker in `rbs-sources`); they may block, read fonts and
//! allocate. This crate links no font library: adapters such as `rbs-text-parley`
//! implement the trait, and `rbs-text-test` is a deterministic fake.

pub mod caps;
pub mod conformance;

use std::fmt;

use rbs_core::capability::CapabilitySet;

/// Largest side of a text box, in pixels (ADR-0129 decision 3).
pub const MAX_SIDE: u32 = 4096;
/// Largest box area, in pixels: 32 MiB of `Rgba8` (ADR-0129 decision 3).
pub const MAX_PIXELS: u64 = 8 * 1024 * 1024;
/// Most characters in a text (ADR-0129 decision 3).
pub const MAX_CHARS: usize = 10_000;
/// Smallest and largest font size, in pixels.
pub const SIZE_RANGE: (f32, f32) = (1.0, 1024.0);
/// Smallest and largest font weight.
pub const WEIGHT_RANGE: (u16, u16) = (100, 900);
/// Smallest and largest line spacing multiplier.
pub const LINE_SPACING_RANGE: (f32, f32) = (0.5, 3.0);
/// Smallest and largest extra letter spacing, in pixels.
pub const LETTER_SPACING_RANGE: (f32, f32) = (-50.0, 200.0);
/// Largest outline width and shadow blur, in pixels.
pub const MAX_STROKE: f32 = 64.0;
/// Largest shadow offset on each axis, in pixels.
pub const MAX_SHADOW_OFFSET: f32 = 256.0;
/// Largest padding, in pixels.
pub const MAX_PADDING: f32 = 1024.0;
/// Smallest font scale `Overflow::Fit` shrinks to.
pub const FIT_MIN_SCALE: f32 = 0.25;
/// Most layouts the `Overflow::Fit` search may try.
pub const FIT_MAX_STEPS: u32 = 8;

/// A straight-alpha sRGB colour.
pub type Rgba = [u8; 4];

/// Horizontal alignment of each line inside the box.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    /// Lines start at the left edge (the right edge for right-to-left text).
    Left,
    /// Lines are centred.
    #[default]
    Center,
    /// Lines end at the right edge.
    Right,
}

/// Vertical placement of the whole text inside the box.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VAlign {
    /// At the top.
    Top,
    /// Centred.
    #[default]
    Middle,
    /// At the bottom.
    Bottom,
}

/// What happens to text that does not fit the box.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overflow {
    /// Shrink the font until the text fits, down to [`FIT_MIN_SCALE`], then clip.
    #[default]
    Fit,
    /// Cut at the box edges.
    Clip,
    /// End the last line that fits with "…".
    Ellipsis,
}

/// The text's fill: one colour, or a linear gradient from `color` to `color2`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fill {
    /// The colour, or the gradient's start.
    pub color: Rgba,
    /// The gradient's end and its direction in degrees (0 = top to bottom, 90 = left
    /// to right), as the colour source draws it.
    pub gradient: Option<(Rgba, f32)>,
}

/// A stroke around each glyph, drawn under the fill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Outline {
    /// Width in pixels, `0.0 < width <= MAX_STROKE`.
    pub width: f32,
    /// Colour.
    pub color: Rgba,
}

/// A blurred, offset copy of the text's silhouette, drawn under everything else.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    /// Colour.
    pub color: Rgba,
    /// Offset in pixels, each axis within `±MAX_SHADOW_OFFSET`.
    pub offset: [f32; 2],
    /// Blur radius in pixels, `0.0..=MAX_STROKE`.
    pub blur: f32,
}

/// Everything a renderer needs to draw one text source.
///
/// Built with [`TextBlock::new`] and then fields set directly; call
/// [`TextBlock::validate`] before handing it to a renderer. New fields arrive with
/// defaults in a minor version of this port.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct TextBlock {
    /// The text; `\n` breaks a line.
    pub text: String,
    /// Font family; `None` is the system UI font.
    pub font: Option<String>,
    /// Font size in pixels.
    pub size: f32,
    /// Font weight, 100 to 900.
    pub weight: u16,
    /// Italic.
    pub italic: bool,
    /// Fill.
    pub fill: Fill,
    /// The picture's size in pixels: `[width, height]`.
    pub box_size: [u32; 2],
    /// Horizontal alignment.
    pub align: Align,
    /// Vertical alignment.
    pub valign: VAlign,
    /// Break lines at the layout width.
    pub wrap: bool,
    /// What happens to text that does not fit.
    pub overflow: Overflow,
    /// Line height as a multiple of the font's.
    pub line_spacing: f32,
    /// Extra space between letters, in pixels.
    pub letter_spacing: f32,
    /// Outline, if any.
    pub outline: Option<Outline>,
    /// Shadow, if any.
    pub shadow: Option<Shadow>,
    /// Background colour of the whole box, if any.
    pub background: Option<Rgba>,
    /// Space between the box edge and the layout area, in pixels.
    pub padding: f32,
    /// Use tabular (equal-width) figures when the font has them, so a clock does not
    /// shimmer.
    pub tabular_figures: bool,
}

impl TextBlock {
    /// `text` in a box of `box_size`, white, in the system UI font at 72 px, centred,
    /// wrapped, shrunk to fit.
    pub fn new(text: impl Into<String>, box_size: [u32; 2]) -> Self {
        Self {
            text: text.into(),
            font: None,
            size: 72.0,
            weight: 400,
            italic: false,
            fill: Fill {
                color: [255, 255, 255, 255],
                gradient: None,
            },
            box_size,
            align: Align::default(),
            valign: VAlign::default(),
            wrap: true,
            overflow: Overflow::default(),
            line_spacing: 1.0,
            letter_spacing: 0.0,
            outline: None,
            shadow: None,
            background: None,
            padding: 0.0,
            tabular_figures: false,
        }
    }

    /// Checks every field against its range. A renderer may assume a validated block.
    pub fn validate(&self) -> Result<(), TextError> {
        let bad = |key: &'static str| Err(TextError::InvalidSetting { key });
        let [w, h] = self.box_size;
        if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
            return bad("width");
        }
        if u64::from(w) * u64::from(h) > MAX_PIXELS {
            return bad("height");
        }
        if self.text.chars().count() > MAX_CHARS {
            return bad("text");
        }
        if !in_range(self.size, SIZE_RANGE) {
            return bad("size");
        }
        if !(WEIGHT_RANGE.0..=WEIGHT_RANGE.1).contains(&self.weight) {
            return bad("weight");
        }
        if self
            .fill
            .gradient
            .is_some_and(|(_, angle)| !angle.is_finite())
        {
            return bad("angle");
        }
        if !in_range(self.line_spacing, LINE_SPACING_RANGE) {
            return bad("line_spacing");
        }
        if !in_range(self.letter_spacing, LETTER_SPACING_RANGE) {
            return bad("letter_spacing");
        }
        if self
            .outline
            .is_some_and(|o| !(o.width.is_finite() && o.width > 0.0 && o.width <= MAX_STROKE))
        {
            return bad("outline_width");
        }
        if let Some(s) = self.shadow {
            let off = (-MAX_SHADOW_OFFSET, MAX_SHADOW_OFFSET);
            if !in_range(s.offset[0], off) {
                return bad("shadow_x");
            }
            if !in_range(s.offset[1], off) {
                return bad("shadow_y");
            }
            if !in_range(s.blur, (0.0, MAX_STROKE)) {
                return bad("shadow_blur");
            }
        }
        if !in_range(self.padding, (0.0, MAX_PADDING)) {
            return bad("padding");
        }
        Ok(())
    }
}

fn in_range(v: f32, (lo, hi): (f32, f32)) -> bool {
    v.is_finite() && v >= lo && v <= hi
}

/// A drawn text: straight-alpha sRGB `Rgba8`, `width * height * 4` bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct TextPicture {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl TextPicture {
    /// A picture of `width` x `height` from `rgba`; `None` when the length is wrong or a
    /// side is zero.
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Option<Self> {
        let expected = u64::from(width) * u64::from(height) * 4;
        (width > 0 && height > 0 && u64::try_from(rgba.len()).ok() == Some(expected)).then_some(
            Self {
                width,
                height,
                rgba,
            },
        )
    }

    /// A fully transparent picture.
    pub fn transparent(width: u32, height: u32) -> Option<Self> {
        let len = usize::try_from(u64::from(width) * u64::from(height) * 4).ok()?;
        Self::new(width, height, vec![0; len])
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The pixels, row by row.
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// Takes the pixels.
    pub fn into_rgba(self) -> Vec<u8> {
        self.rgba
    }

    /// The pixel at `(x, y)`, or `None` outside the picture.
    pub fn pixel(&self, x: u32, y: u32) -> Option<Rgba> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y as usize * self.width as usize + x as usize) * 4;
        let p = self.rgba.get(i..i + 4)?;
        Some([p[0], p[1], p[2], p[3]])
    }
}

impl fmt::Debug for TextPicture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextPicture")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// Why a block could not be measured or drawn.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextError {
    /// A field is outside its range; `key` is the source setting that holds it.
    InvalidSetting {
        /// The setting key.
        key: &'static str,
    },
    /// The renderer failed. The message never holds a path or the text itself.
    Render(String),
}

impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TextError::InvalidSetting { key } => write!(f, "the setting `{key}` is out of range"),
            TextError::Render(message) => write!(f, "text could not be drawn: {message}"),
        }
    }
}

impl std::error::Error for TextError {}

/// What a renderer reports next to a picture.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RenderNotes {
    /// The requested family is not installed; the system UI font was used.
    pub font_missing: bool,
}

/// Lays out and draws text. Off the render thread only.
pub trait TextRenderer: Send + Sync {
    /// A stable id for logs and settings, for example `parley`.
    fn id(&self) -> &str;

    /// What this renderer can do ([`caps`]).
    fn capabilities(&self) -> &CapabilitySet;

    /// Draws `block` into a picture of exactly `block.box_size`. `block` must be valid
    /// ([`TextBlock::validate`]); an invalid block is `InvalidSetting`.
    fn render(&self, block: &TextBlock) -> Result<(TextPicture, RenderNotes), TextError>;

    /// The natural size of `block`'s text in pixels, padding, outline and shadow
    /// included: on one line per `\n` when `max_width` is `None`, else wrapped at
    /// `max_width`. Ignores `box_size` and `overflow`.
    fn measure(&self, block: &TextBlock, max_width: Option<u32>) -> Result<[u32; 2], TextError>;

    /// Installed font families, sorted, without duplicates.
    fn families(&self) -> Result<Vec<String>, TextError>;

    /// Drops font caches; called when no text source is left. The next call builds
    /// them again.
    fn trim(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_block_is_valid() {
        assert_eq!(TextBlock::new("x", [10, 10]).validate(), Ok(()));
    }

    #[test]
    fn each_range_names_its_setting() {
        let base = TextBlock::new("x", [10, 10]);
        type Edit = fn(&mut TextBlock);
        let cases: [(&str, Edit); 9] = [
            ("width", |b| b.box_size = [MAX_SIDE + 1, 10]),
            ("height", |b| b.box_size = [MAX_SIDE, MAX_SIDE]),
            ("size", |b| b.size = f32::NAN),
            ("weight", |b| b.weight = 950),
            ("line_spacing", |b| b.line_spacing = 0.1),
            ("letter_spacing", |b| b.letter_spacing = 500.0),
            ("padding", |b| b.padding = -1.0),
            ("outline_width", |b| {
                b.outline = Some(Outline {
                    width: 0.0,
                    color: [0; 4],
                })
            }),
            ("shadow_blur", |b| {
                b.shadow = Some(Shadow {
                    color: [0; 4],
                    offset: [0.0, 0.0],
                    blur: 65.0,
                })
            }),
        ];
        for (key, edit) in cases {
            let mut b = base.clone();
            edit(&mut b);
            assert_eq!(
                b.validate(),
                Err(TextError::InvalidSetting { key }),
                "{key}"
            );
        }
        let mut long = base;
        long.text = "a".repeat(MAX_CHARS + 1);
        assert_eq!(
            long.validate(),
            Err(TextError::InvalidSetting { key: "text" })
        );
    }

    #[test]
    fn a_picture_checks_its_length() {
        assert!(TextPicture::new(2, 2, vec![0; 16]).is_some());
        assert!(TextPicture::new(2, 2, vec![0; 15]).is_none());
        assert!(TextPicture::new(0, 2, Vec::new()).is_none());
    }
}
