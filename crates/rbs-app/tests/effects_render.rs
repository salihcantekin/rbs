//! Theme effects never cover a focus ring (ADR-0120 decision 11): the rows of the
//! library spacing, rendered with Skia's real pixels, in every setting and in regular and
//! compact density. Its own test binary, because the platform (here the testing backend
//! with the Skia renderer) is set once per process.
//!
//! A ring is the only thing drawn in `color.border.focus`. Each control is focused in
//! turn; every pixel that shows the ring in calm must show it unchanged with effects, so
//! no neighbour's shadow, glow or key edge lies over any part of it.

#![allow(clippy::expect_used, clippy::unwrap_used)] // Tests fail by panicking.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{EffectsProbe, Theme, Tokens};
use rbs_ui::tokens::{Base, Density, Effects, ThemeBody, Tokens as TokenFile, apply_density};
use slint::ComponentHandle;

fn init() {
    let backend = i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            renderer_name: Some("skia".into()),
            ..Default::default()
        },
    );
    slint::platform::set_platform(Box::new(backend)).expect("the testing platform");
}

const FOCUS: [fn(&EffectsProbe); 8] = [
    EffectsProbe::invoke_focus_mid,
    EffectsProbe::invoke_focus_go,
    EffectsProbe::invoke_focus_go_menu,
    EffectsProbe::invoke_focus_record,
    EffectsProbe::invoke_focus_record_menu,
    EffectsProbe::invoke_focus_icon,
    EffectsProbe::invoke_focus_above,
    EffectsProbe::invoke_focus_below,
];

/// The probe with `effects` on the Dark theme at `density`, `focus` given its ring,
/// rendered: the pixels and the ring colour.
fn render(
    file: &TokenFile,
    effects: Effects,
    density: Density,
    focus: fn(&EffectsProbe),
) -> (Vec<[u8; 4]>, usize, [u8; 4]) {
    let mut body = ThemeBody::new(Base::Dark);
    body.set_effects(effects);
    let mut theme = file.resolve(&body).expect("the theme resolves");
    assert_eq!(theme.effects, effects);
    apply_density(&mut theme.length, density, 1);
    let s = slot(&theme, "").expect("a slot");
    let p = EffectsProbe::new().expect("the probe window");
    apply_tokens(
        &p.global::<Theme>(),
        &p.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .expect("the tokens apply");
    p.show().expect("show");
    focus(&p);
    slint::platform::update_timers_and_animations();
    let shot = p.window().take_snapshot().expect("a snapshot");
    let width = shot.width() as usize;
    let pixels = shot
        .as_slice()
        .iter()
        .map(|px| [px.r, px.g, px.b, px.a])
        .collect();
    p.hide().expect("hide");
    let f = theme.colors["color.border.focus"];
    (pixels, width, [f.r, f.g, f.b, 255])
}

/// The pixels the ring encloses (not reachable from the image's edge without crossing
/// a ring pixel) within `reach` pixels of the ring: what lies under and beside the ring
/// on the control's side.
fn beside_ring(ring: &[bool], width: usize, reach: usize) -> Vec<usize> {
    let height = ring.len() / width;
    let mut outside = vec![false; ring.len()];
    let mut stack: Vec<usize> = (0..ring.len())
        .filter(|i| {
            let (x, y) = (i % width, i / width);
            x == 0 || y == 0 || x == width - 1 || y == height - 1
        })
        .filter(|i| !ring[*i])
        .collect();
    while let Some(i) = stack.pop() {
        if outside[i] || ring[i] {
            continue;
        }
        outside[i] = true;
        let (x, y) = (i % width, i / width);
        if x > 0 {
            stack.push(i - 1);
        }
        if x + 1 < width {
            stack.push(i + 1);
        }
        if y > 0 {
            stack.push(i - width);
        }
        if y + 1 < height {
            stack.push(i + width);
        }
    }
    (0..ring.len())
        .filter(|i| !ring[*i] && !outside[*i])
        .filter(|i| {
            let (x, y) = ((i % width) as isize, (i / width) as isize);
            let r = reach as isize;
            (-r..=r).any(|dy| {
                (-r..=r).any(|dx| {
                    let (nx, ny) = (x + dx, y + dy);
                    nx >= 0
                        && ny >= 0
                        && (nx as usize) < width
                        && (ny as usize) < height
                        && ring[ny as usize * width + nx as usize]
                })
            })
        })
        .collect()
}

/// One test, because the platform is set once per process.
#[test]
fn effects_keep_every_focus_ring_and_every_size() {
    init();
    let file = TokenFile::built_in().unwrap();
    effects_never_change_the_layout(&file);
    for density in [Density::Regular, Density::Compact] {
        for (n, focus) in FOCUS.iter().enumerate() {
            let (calm, width, ring) = render(&file, Effects::Calm, density, *focus);
            let is_ring: Vec<bool> = calm.iter().map(|p| *p == ring).collect();
            let ring_pixels: Vec<usize> = (0..calm.len()).filter(|i| is_ring[*i]).collect();
            let beside = beside_ring(&is_ring, width, 2);
            assert!(
                ring_pixels.len() > 40,
                "control {n} shows no ring in calm ({density:?})"
            );
            for effects in Effects::LIVELY {
                let (lively, _, _) = render(&file, effects, density, *focus);
                let changed = beside.iter().filter(|i| lively[**i] != calm[**i]).count();
                assert_eq!(
                    changed, 0,
                    "{changed} pixels beside the ring of control {n} change with {effects:?} ({density:?})"
                );
                let covered = ring_pixels.iter().filter(|i| lively[**i] != ring).count();
                assert_eq!(
                    covered, 0,
                    "{covered} ring pixels of control {n} differ with {effects:?} ({density:?})"
                );
            }
        }
    }
}

/// Effects never change a control's size or place (decision 11): every control of the
/// probe has the same geometry in every setting and density.
fn effects_never_change_the_layout(file: &TokenFile) {
    for density in [Density::Regular, Density::Compact, Density::Roomy] {
        let geometry = |effects| {
            let mut body = ThemeBody::new(Base::Light);
            body.set_effects(effects);
            let mut theme = file.resolve(&body).unwrap();
            apply_density(&mut theme.length, density, 1);
            let s = slot(&theme, "").unwrap();
            let p = EffectsProbe::new().unwrap();
            apply_tokens(
                &p.global::<Theme>(),
                &p.global::<Tokens>(),
                file,
                [s.clone(), s],
            )
            .unwrap();
            p.get_geometry()
        };
        let calm = geometry(Effects::Calm);
        for e in Effects::LIVELY {
            assert_eq!(geometry(e), calm, "{e:?} {density:?}");
        }
    }
}
