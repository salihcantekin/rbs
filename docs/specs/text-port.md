# Spec: Text rendering port

- Status: Draft
- Owner crate(s): `rbs-text` (port, conformance), `rbs-text-parley` (adapter), `rbs-text-test` (fake)
- Related ADRs: [0129](../adr/0129-text-source.md) (Proposed), [0013](../adr/0013-zero-cost-when-unused.md), [0032](../adr/0032-fast-deterministic-tests.md)

## Purpose

Lays out a block of text and draws it into a picture of a fixed size, for the text source (ADR-0129). The engine and the text worker see only this trait, so the font library and the system font list stay in an adapter (P1, P2).

## Scope

In scope: the block's settings as typed values, their ranges, drawing into exactly the box, measuring the natural size, listing installed families, dropping caches. Out of scope (ADR-0129, later phases): the source type and settings keys (`scene-sources.md`), the node and upload, the text worker, files, live kinds, scrolling.

## Contract

```rust
// rbs_text
pub const MAX_SIDE: u32 = 4096;            // box side, px
pub const MAX_PIXELS: u64 = 8 * 1024 * 1024; // box area, 32 MiB of Rgba8
pub const MAX_CHARS: usize = 10_000;
pub const FIT_MIN_SCALE: f32 = 0.25;       // Overflow::Fit shrinks to this at most
pub const FIT_MAX_STEPS: u32 = 8;          // layouts the Fit search may try

#[non_exhaustive] pub enum Align { Left, Center /* default */, Right }
#[non_exhaustive] pub enum VAlign { Top, Middle /* default */, Bottom }
#[non_exhaustive] pub enum Overflow { Fit /* default */, Clip, Ellipsis }
pub struct Fill { pub color: Rgba, pub gradient: Option<(Rgba, f32 /* degrees */)> }
pub struct Outline { pub width: f32, pub color: Rgba }
pub struct Shadow { pub color: Rgba, pub offset: [f32; 2], pub blur: f32 }

#[non_exhaustive]
pub struct TextBlock {
    pub text: String, pub font: Option<String>, pub size: f32, pub weight: u16,
    pub italic: bool, pub fill: Fill, pub box_size: [u32; 2], pub align: Align,
    pub valign: VAlign, pub wrap: bool, pub overflow: Overflow, pub line_spacing: f32,
    pub letter_spacing: f32, pub outline: Option<Outline>, pub shadow: Option<Shadow>,
    pub background: Option<Rgba>, pub padding: f32, pub tabular_figures: bool,
}
impl TextBlock {
    pub fn new(text: impl Into<String>, box_size: [u32; 2]) -> Self; // white, 72 px, system UI font
    pub fn validate(&self) -> Result<(), TextError>;
}

pub struct TextPicture { /* width, height, straight-alpha sRGB Rgba8 */ }
#[non_exhaustive] pub enum TextError { InvalidSetting { key: &'static str }, Render(String) }
#[non_exhaustive] pub struct RenderNotes { pub font_missing: bool }

pub trait TextRenderer: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> &CapabilitySet;   // rbs_text::caps
    fn render(&self, block: &TextBlock) -> Result<(TextPicture, RenderNotes), TextError>;
    fn measure(&self, block: &TextBlock, max_width: Option<u32>) -> Result<[u32; 2], TextError>;
    fn families(&self) -> Result<Vec<String>, TextError>;
    fn trim(&self) {}
}
```

Ranges (`validate`, each refusal names the source setting): box sides 1 to 4096 and area at most 8 388 608 pixels (`width`, `height`); at most 10 000 characters (`text`); `size` 1 to 1024; `weight` 100 to 900; gradient `angle` finite; `line_spacing` 0.5 to 3; `letter_spacing` -50 to 200; outline width above 0 and at most 64 (`outline_width`); shadow offsets within ±256 (`shadow_x`, `shadow_y`) and blur 0 to 64 (`shadow_blur`); `padding` 0 to 1024. Non-finite numbers are refused.

Capabilities (`rbs_text::caps`, flags): `text.color_glyphs` (emoji in colour), `text.tabular_figures` (honours `tabular_figures`), `text.system_fonts` (lists the system's families).

Invariants:

1. `render` of a valid block returns a picture of exactly `box_size`; an invalid block is `InvalidSetting` and draws nothing.
2. The layout area is the box less `padding` on each side and less the space the outline and shadow take (outline width; shadow blur plus offset on that side). Outline and shadow are drawn inside the box.
3. Draw order: background over the whole box, the shadow (the blurred silhouette of fill and outline, moved by the offset), the outline, the fill (colour or gradient across the box, 0 degrees top to bottom as the colour source), colour glyphs.
4. `wrap` breaks lines at the area width. With `wrap` off each `\n` starts a line and the block of lines is placed in the area by `align`; with `wrap` on each line is aligned within the area width. `valign` places the block of lines in the area height.
5. `Fit` shrinks the font (and letter spacing) by a binary search of at most `FIT_MAX_STEPS` layouts down to `FIT_MIN_SCALE`, then clips. `Clip` cuts at the box edges. `Ellipsis` behaves as `Clip` in the first adapter step; the "…" ending is a follow-up.
6. A family that is not installed is drawn in the default family with `RenderNotes::font_missing`; it is never an error. `font: None` is the system UI font (`rbs-text-parley` with system fonts) or the first font a renderer was given.
7. `measure` returns the natural size of the text at full size, padding, outline and shadow included, wrapped at `max_width` (the text area's width) when given. It ignores `box_size` and `overflow`.
8. `families` is sorted case-insensitively without duplicates.
9. `trim` drops font caches and contexts; the next call builds them again and draws the same pictures.
10. Renderers are called off the render thread only (the text worker). They may block and allocate. Error messages never contain a path or the text.

## Errors and failure behaviour

`InvalidSetting { key }` for a value outside its range; `Render(message)` for a renderer failure. The caller (the text worker, ADR-0129 decision 7) wraps each call in `catch_unwind`, keeps the source's last picture and shows the problem on the source.

## Performance requirements

Not real time. `rbs-text-parley` builds its font collection lazily on the first call and holds nothing before it (P13, Installed state). Measured in release on the cloud build machine (2026-11-16, `examples/text_sample.rs`): a 1200 x 130 title with outline and blurred shadow in 21 ms, a 620 x 170 two-line plate in 4 ms, a 560 x 170 clock in 3 ms.

## Conformance tests

`rbs_text::conformance::run(&dyn TextRenderer)`: the picture is the box for several sizes; an empty text is transparent; a text draws ink; the background reaches every corner; left, centre and right move the ink in that order; `Fit` shrinks a line that is too long; an outline adds pixels; size 0 and a zero-width box are refused; `measure` is repeatable, wraps when narrower and adds the padding exactly; the same block draws the same picture twice. `rbs-text-test` and `rbs-text-parley` run it; the parley tests load the bundled font `crates/rbs-text-parley/tests/fonts/inter-subset.otf` (a subset of Inter, SIL OFL 1.1, licence beside it) with no system fonts, so they give the same result on every machine.
