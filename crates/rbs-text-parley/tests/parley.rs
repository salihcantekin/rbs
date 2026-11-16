//! The parley renderer on the bundled test font (a subset of Inter, SIL OFL 1.1, see
//! `tests/fonts/OFL.txt`), so results do not depend on the machine's fonts (ADR-0032).

use rbs_text::conformance::{self, ink_bounds};
use rbs_text::{Outline, Shadow, TextBlock, TextRenderer};
use rbs_text_parley::ParleyRenderer;

const FONT: &[u8] = include_bytes!("fonts/inter-subset.otf");

fn renderer() -> ParleyRenderer {
    ParleyRenderer::with_fonts(vec![FONT.to_vec()])
}

#[test]
fn passes_the_port_conformance_suite() {
    assert_eq!(conformance::run(&renderer()), Ok(()));
}

#[test]
fn lists_only_the_given_font() {
    assert_eq!(renderer().families(), Ok(vec!["Inter".to_owned()]));
}

#[test]
fn a_missing_family_falls_back_with_a_note() {
    let r = renderer();
    let mut b = TextBlock::new("Merhaba", [300, 80]);
    b.font = Some("No Such Family".to_owned());
    let (fallback, notes) = r.render(&b).unwrap_or_else(|e| panic!("{e}"));
    assert!(notes.font_missing);
    b.font = None;
    let (default, notes) = r.render(&b).unwrap_or_else(|e| panic!("{e}"));
    assert!(!notes.font_missing);
    assert_eq!(fallback, default);
}

#[test]
fn shadow_lands_below_and_right_of_the_text() {
    let r = renderer();
    let mut b = TextBlock::new("I", [120, 120]);
    b.size = 60.0;
    let plain = ink_bounds(&r.render(&b).unwrap_or_else(|e| panic!("{e}")).0);
    b.shadow = Some(Shadow {
        color: [0, 0, 0, 255],
        offset: [10.0, 10.0],
        blur: 0.0,
    });
    let shadowed = ink_bounds(&r.render(&b).unwrap_or_else(|e| panic!("{e}")).0);
    let (Some(p), Some(s)) = (plain, shadowed) else {
        panic!("nothing drawn");
    };
    // The layout area shrinks by the shadow's extent, so compare the far edges.
    assert!(s[2] > p[2] && s[3] > p[3], "{p:?} vs {s:?}");
}

#[test]
fn trim_drops_the_fonts_and_the_next_call_builds_them_again() {
    let r = renderer();
    let b = TextBlock::new("12:00", [200, 80]);
    let first = r.render(&b).unwrap_or_else(|e| panic!("{e}")).0;
    r.trim();
    let second = r.render(&b).unwrap_or_else(|e| panic!("{e}")).0;
    assert_eq!(first, second);
}

#[test]
fn outline_reaches_past_the_fill() {
    let r = renderer();
    let mut b = TextBlock::new("H", [200, 200]);
    b.size = 100.0;
    b.outline = Some(Outline {
        width: 8.0,
        color: [0, 0, 0, 255],
    });
    let (p, _) = r.render(&b).unwrap_or_else(|e| panic!("{e}"));
    // Some pixel must be the outline colour, opaque.
    assert!(p.rgba().chunks_exact(4).any(|px| px == [0, 0, 0, 255]));
    assert!(
        p.rgba()
            .chunks_exact(4)
            .any(|px| px == [255, 255, 255, 255])
    );
}
