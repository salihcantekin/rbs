//! A deterministic fake [`TextRenderer`] (ADR-0129): no fonts, every character is a
//! box from fixed metrics, so tests get the same pictures on every machine.
//!
//! Metrics at font size `s`: advance `0.6 s` plus letter spacing, line height
//! `1.2 s` times line spacing; a non-space character inks the rectangle from `0.1 s`
//! to `0.5 s` across and `0.2 s` to `0.9 s` down its cell. Words wrap greedily at
//! spaces. `Ellipsis` behaves as `Clip`, the gradient draws its first colour and the
//! shadow is not blurred.

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_text::{
    FIT_MAX_STEPS, FIT_MIN_SCALE, Overflow, RenderNotes, Rgba, TextBlock, TextError, TextPicture,
    TextRenderer, VAlign, caps,
};

/// Families the fake reports as installed.
pub const FAMILIES: [&str; 2] = ["Fake Sans", "Fake Serif"];

/// The fake renderer.
#[derive(Debug)]
pub struct FakeTextRenderer {
    caps: CapabilitySet,
}

impl Default for FakeTextRenderer {
    fn default() -> Self {
        Self {
            caps: CapabilitySet::new()
                .with(caps::TABULAR_FIGURES, CapabilityValue::Flag(true))
                .with(caps::COLOR_GLYPHS, CapabilityValue::Flag(false))
                .with(caps::SYSTEM_FONTS, CapabilityValue::Flag(false)),
        }
    }
}

impl FakeTextRenderer {
    /// A new fake.
    pub fn new() -> Self {
        Self::default()
    }
}

struct Metrics {
    advance: f32,
    line: f32,
    size: f32,
}

impl Metrics {
    fn of(block: &TextBlock, scale: f32) -> Self {
        let size = block.size * scale;
        Self {
            advance: (0.6 * size + block.letter_spacing).max(0.0),
            line: 1.2 * size * block.line_spacing,
            size,
        }
    }
}

/// The lines of `text`, wrapped at `width` when given.
fn lines(text: &str, m: &Metrics, width: Option<f32>) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let Some(width) = width else {
            out.push(para.to_owned());
            continue;
        };
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if !line.is_empty() && candidate.chars().count() as f32 * m.advance > width {
                out.push(std::mem::take(&mut line));
                line = word.to_owned();
            } else {
                line = candidate;
            }
        }
        out.push(line);
    }
    out
}

fn text_size(lines: &[String], m: &Metrics) -> (f32, f32) {
    let w = lines
        .iter()
        .map(|l| l.chars().count() as f32 * m.advance)
        .fold(0.0, f32::max);
    (w, lines.len() as f32 * m.line)
}

/// Space outline and shadow take around the text: `[left, top, right, bottom]`.
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
    e.map(|v| v.max(0.0))
}

struct Canvas {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

impl Canvas {
    fn rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, c: Rgba) {
        let clamp = |v: f32, max: u32| v.round().clamp(0.0, max as f32) as u32;
        let (x0, x1) = (clamp(x0, self.w), clamp(x1, self.w));
        let (y0, y1) = (clamp(y0, self.h), clamp(y1, self.h));
        for y in y0..y1 {
            for x in x0..x1 {
                let i = (y as usize * self.w as usize + x as usize) * 4;
                if let Some(p) = self.px.get_mut(i..i + 4) {
                    p.copy_from_slice(&c);
                }
            }
        }
    }
}

impl TextRenderer for FakeTextRenderer {
    fn id(&self) -> &str {
        "fake"
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

        let fits = |scale: f32| {
            let m = Metrics::of(block, scale);
            let (w, h) = text_size(&lines(&block.text, &m, wrap), &m);
            w <= area_w && h <= area_h
        };
        let mut scale = 1.0;
        if block.overflow == Overflow::Fit && !fits(1.0) {
            let (mut lo, mut hi) = (FIT_MIN_SCALE, 1.0);
            for _ in 0..FIT_MAX_STEPS {
                let mid = (lo + hi) / 2.0;
                if fits(mid) { lo = mid } else { hi = mid }
            }
            scale = lo;
        }
        let m = Metrics::of(block, scale);
        let lines = lines(&block.text, &m, wrap);
        let (_, text_h) = text_size(&lines, &m);

        let mut c = Canvas {
            w: bw,
            h: bh,
            px: vec![0; bw as usize * bh as usize * 4],
        };
        if let Some(bg) = block.background {
            c.rect(0.0, 0.0, bw as f32, bh as f32, bg);
        }
        let left = block.padding + ext[0];
        let top = block.padding
            + ext[1]
            + match block.valign {
                VAlign::Top => 0.0,
                VAlign::Bottom => area_h - text_h,
                _ => (area_h - text_h) / 2.0,
            };
        // Glyph ink rectangles, in box pixels.
        let mut glyphs = Vec::new();
        for (row, line) in lines.iter().enumerate() {
            let line_w = line.chars().count() as f32 * m.advance;
            let x = left
                + match block.align {
                    rbs_text::Align::Left => 0.0,
                    rbs_text::Align::Right => area_w - line_w,
                    _ => (area_w - line_w) / 2.0,
                };
            let y = top + row as f32 * m.line;
            for (i, ch) in line.chars().enumerate() {
                if ch.is_whitespace() {
                    continue;
                }
                let gx = x + i as f32 * m.advance;
                glyphs.push([
                    gx + 0.1 * m.size,
                    y + 0.2 * m.size,
                    gx + 0.5 * m.size,
                    y + 0.9 * m.size,
                ]);
            }
        }
        let o = block.outline.map_or(0.0, |o| o.width);
        if let Some(s) = block.shadow {
            for g in &glyphs {
                c.rect(
                    g[0] - o + s.offset[0],
                    g[1] - o + s.offset[1],
                    g[2] + o + s.offset[0],
                    g[3] + o + s.offset[1],
                    s.color,
                );
            }
        }
        if let Some(outline) = block.outline {
            for g in &glyphs {
                c.rect(g[0] - o, g[1] - o, g[2] + o, g[3] + o, outline.color);
            }
        }
        for g in &glyphs {
            c.rect(g[0], g[1], g[2], g[3], block.fill.color);
        }
        let picture = TextPicture::new(bw, bh, c.px)
            .ok_or_else(|| TextError::Render("picture size".to_owned()))?;
        let font_missing = block
            .font
            .as_deref()
            .is_some_and(|f| !FAMILIES.contains(&f));
        let mut notes = RenderNotes::default();
        notes.font_missing = font_missing;
        Ok((picture, notes))
    }

    fn measure(&self, block: &TextBlock, max_width: Option<u32>) -> Result<[u32; 2], TextError> {
        let mut probe = block.clone();
        probe.box_size = [1, 1];
        probe.validate()?;
        let m = Metrics::of(block, 1.0);
        let ext = extents(block);
        let (w, h) = text_size(&lines(&block.text, &m, max_width.map(|w| w as f32)), &m);
        let pad = 2.0 * block.padding;
        Ok([
            (w + pad + ext[0] + ext[2]).ceil() as u32,
            (h + pad + ext[1] + ext[3]).ceil() as u32,
        ])
    }

    fn families(&self) -> Result<Vec<String>, TextError> {
        Ok(FAMILIES.iter().map(|f| (*f).to_owned()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_the_port_conformance_suite() {
        assert_eq!(rbs_text::conformance::run(&FakeTextRenderer::new()), Ok(()));
    }

    #[test]
    fn a_missing_family_is_a_note() {
        let mut b = TextBlock::new("x", [20, 20]);
        b.font = Some("Nope".to_owned());
        let (_, notes) = FakeTextRenderer::new()
            .render(&b)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(notes.font_missing);
    }
}
