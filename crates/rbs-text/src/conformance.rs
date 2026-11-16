//! Conformance checks every [`TextRenderer`] must pass (ADR-0129, spec
//! `docs/specs/text-port.md`). Adapters call [`run`] from their tests. The checks use
//! the renderer's default font, so they hold for the fake and for a real renderer with
//! any font; they compare pictures with each other, never with stored pixels.

use crate::{Align, Outline, TextBlock, TextError, TextPicture, TextRenderer};

/// Runs every check; the first failure is returned as a sentence.
pub fn run(renderer: &dyn TextRenderer) -> Result<(), String> {
    picture_is_the_box(renderer)?;
    empty_text_is_transparent(renderer)?;
    text_draws_ink(renderer)?;
    background_fills_the_box(renderer)?;
    alignment_moves_the_ink(renderer)?;
    fit_keeps_the_ink_inside(renderer)?;
    outline_adds_ink(renderer)?;
    invalid_block_is_refused(renderer)?;
    measure_is_stable_and_wraps(renderer)?;
    renders_are_repeatable(renderer)?;
    Ok(())
}

fn draw(r: &dyn TextRenderer, block: &TextBlock) -> Result<TextPicture, String> {
    r.render(block)
        .map(|(picture, _)| picture)
        .map_err(|e| format!("render failed: {e}"))
}

/// Bounding box of pixels with alpha above zero: `[x0, y0, x1, y1]` (exclusive end).
pub fn ink_bounds(p: &TextPicture) -> Option<[u32; 4]> {
    let mut b: Option<[u32; 4]> = None;
    for y in 0..p.height() {
        for x in 0..p.width() {
            if p.pixel(x, y).is_some_and(|px| px[3] > 0) {
                let e = b.get_or_insert([x, y, x + 1, y + 1]);
                e[0] = e[0].min(x);
                e[1] = e[1].min(y);
                e[2] = e[2].max(x + 1);
                e[3] = e[3].max(y + 1);
            }
        }
    }
    b
}

fn ink_count(p: &TextPicture) -> usize {
    p.rgba().chunks_exact(4).filter(|px| px[3] > 0).count()
}

fn picture_is_the_box(r: &dyn TextRenderer) -> Result<(), String> {
    for size in [[1, 1], [320, 90], [97, 403], [1000, 40]] {
        let p = draw(r, &TextBlock::new("Hello, dünya", size))?;
        if [p.width(), p.height()] != size {
            return Err(format!(
                "a {size:?} box gave a {}x{} picture",
                p.width(),
                p.height()
            ));
        }
    }
    Ok(())
}

fn empty_text_is_transparent(r: &dyn TextRenderer) -> Result<(), String> {
    let p = draw(r, &TextBlock::new("", [64, 32]))?;
    if ink_count(&p) != 0 {
        return Err("an empty text drew pixels".to_owned());
    }
    Ok(())
}

fn text_draws_ink(r: &dyn TextRenderer) -> Result<(), String> {
    let mut b = TextBlock::new("Hg İş", [400, 120]);
    b.size = 48.0;
    let p = draw(r, &b)?;
    if ink_count(&p) == 0 {
        return Err("a text drew nothing".to_owned());
    }
    Ok(())
}

fn background_fills_the_box(r: &dyn TextRenderer) -> Result<(), String> {
    let mut b = TextBlock::new("A", [50, 30]);
    b.background = Some([10, 20, 30, 255]);
    let p = draw(r, &b)?;
    for (x, y) in [(0, 0), (49, 0), (0, 29), (49, 29)] {
        if p.pixel(x, y) != Some([10, 20, 30, 255]) {
            return Err(format!("the background does not reach ({x}, {y})"));
        }
    }
    Ok(())
}

fn alignment_moves_the_ink(r: &dyn TextRenderer) -> Result<(), String> {
    let at = |align: Align| -> Result<[u32; 4], String> {
        let mut b = TextBlock::new("abc", [600, 100]);
        b.size = 40.0;
        b.align = align;
        ink_bounds(&draw(r, &b)?).ok_or_else(|| "a text drew nothing".to_owned())
    };
    let (left, center, right) = (at(Align::Left)?, at(Align::Center)?, at(Align::Right)?);
    if !(left[0] < center[0] && center[0] < right[0]) {
        return Err(format!(
            "left, centre and right start at {}, {} and {}",
            left[0], center[0], right[0]
        ));
    }
    Ok(())
}

fn fit_keeps_the_ink_inside(r: &dyn TextRenderer) -> Result<(), String> {
    let mut b = TextBlock::new("A much too long line for this box", [200, 60]);
    b.size = 200.0;
    b.wrap = false;
    let fitted = draw(r, &b)?;
    let Some(ink) = ink_bounds(&fitted) else {
        return Err("a fitted text drew nothing".to_owned());
    };
    // Fit shrinks to at least FIT_MIN_SCALE; the ink must not be cut on both sides.
    if ink[0] == 0 && ink[2] == 200 && ink[1] == 0 && ink[3] == 60 {
        return Err("Fit did not shrink the text".to_owned());
    }
    Ok(())
}

fn outline_adds_ink(r: &dyn TextRenderer) -> Result<(), String> {
    let mut b = TextBlock::new("O", [200, 200]);
    b.size = 80.0;
    let plain = ink_count(&draw(r, &b)?);
    b.outline = Some(Outline {
        width: 6.0,
        color: [0, 0, 0, 255],
    });
    let outlined = ink_count(&draw(r, &b)?);
    if outlined <= plain {
        return Err("an outline added no pixels".to_owned());
    }
    Ok(())
}

fn invalid_block_is_refused(r: &dyn TextRenderer) -> Result<(), String> {
    let mut b = TextBlock::new("x", [64, 64]);
    b.size = 0.0;
    match r.render(&b) {
        Err(TextError::InvalidSetting { key: "size" }) => {}
        other => return Err(format!("size 0 gave {:?}", other.map(|(p, _)| p))),
    }
    let b = TextBlock::new("x", [0, 64]);
    if r.render(&b).is_ok() {
        return Err("a zero-width box was drawn".to_owned());
    }
    Ok(())
}

fn measure_is_stable_and_wraps(r: &dyn TextRenderer) -> Result<(), String> {
    let mut b = TextBlock::new("one two three four five six", [1, 1]);
    b.size = 30.0;
    let a = r.measure(&b, None).map_err(|e| e.to_string())?;
    let again = r.measure(&b, None).map_err(|e| e.to_string())?;
    if a != again {
        return Err(format!("measure gave {a:?} then {again:?}"));
    }
    let narrow = r.measure(&b, Some(a[0] / 3)).map_err(|e| e.to_string())?;
    if !(narrow[0] <= a[0] && narrow[1] > a[1]) {
        return Err(format!("wrapping at a third gave {narrow:?} from {a:?}"));
    }
    b.padding = 10.0;
    let padded = r.measure(&b, None).map_err(|e| e.to_string())?;
    if padded != [a[0] + 20, a[1] + 20] {
        return Err(format!("padding 10 gave {padded:?} from {a:?}"));
    }
    Ok(())
}

fn renders_are_repeatable(r: &dyn TextRenderer) -> Result<(), String> {
    let mut b = TextBlock::new("12:34:56", [300, 80]);
    b.size = 50.0;
    b.tabular_figures = true;
    if draw(r, &b)? != draw(r, &b)? {
        return Err("the same block drew two different pictures".to_owned());
    }
    Ok(())
}
