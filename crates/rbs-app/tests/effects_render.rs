//! Theme effects never cover a focus ring and never reach another control (ADR-0120
//! decision 11, ADR-0122 decision 10): the rows of the library spacing and the real frame
//! bar, rendered with Skia's real pixels, in Dark and Light, in every setting and in
//! regular and compact density (compact in the slow tier). Its own test binary, because
//! the platform (here the testing backend with the Skia renderer) is set once per process.
//!
//! The ring rule: every ring pixel keeps at least min(3, its calm ratio) against the
//! ground pixel beside it in the same render. A ring pixel is one that turns exactly
//! `color.border.focus` when its control takes the keyboard focus. The ground beside it
//! is every pixel two pixels from the ring (past the anti-aliased fringe): on the
//! control's side always, and on the outer side for a control whose ring sits outside
//! its shape (a SplitButton). The outer side of a Button's ring is the button's own
//! shadow and key edge, not ground. A neighbour's own effects are not ground either:
//! they stay within the extent of that neighbour's box, which the library spacing keeps
//! clear of the ring (ADR-0120 decision 11), so a pixel within the extent of another
//! button's box is left out.

#![allow(clippy::expect_used, clippy::unwrap_used)] // Tests fail by panicking.

use std::cell::RefCell;
use std::rc::Rc;

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{
    EffectsProbe, MainWindow, SegmentProbe, Shell, Theme, ThemeSlot, Tokens, ToolbarData,
};
use rbs_ui::tokens::{
    Base, CONTRAST_RULES, Color, Density, Effects, FACE_KINDS, KIND_CHECKED, KIND_DANGER,
    KIND_PRIMARY, KIND_SECONDARY, ResolvedTheme, THEME_CONTRAST_RULES, ThemeBody,
    Tokens as TokenFile, apply_density, band_top,
};
use slint::ComponentHandle;
use slint::platform::WindowEvent;

type Job = Box<dyn FnOnce() + Send>;

/// Runs `test` on the one thread that owns the platform (the testing backend with the
/// Skia renderer): the platform and its windows belong to the thread that set it, and
/// each test of this binary has its own thread.
fn on_the_platform_thread(test: fn()) {
    static JOBS: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<Job>>> =
        std::sync::OnceLock::new();
    let jobs = JOBS.get_or_init(|| {
        let (send, receive) = std::sync::mpsc::channel::<Job>();
        std::thread::spawn(move || {
            let backend = i_slint_backend_testing::TestingBackend::new(
                i_slint_backend_testing::TestingBackendOptions {
                    renderer_name: Some("skia".into()),
                    ..Default::default()
                },
            );
            slint::platform::set_platform(Box::new(backend)).expect("the testing platform");
            for job in receive {
                job();
            }
        });
        std::sync::Mutex::new(send)
    });
    let (done, result) = std::sync::mpsc::channel();
    jobs.lock()
        .unwrap_or_else(|e| e.into_inner())
        .send(Box::new(move || {
            let _ = done.send(std::panic::catch_unwind(test));
        }))
        .expect("the platform thread runs");
    // No timeout: the wait ends when the job ends, or with an error if its thread died.
    if let Err(panic) = result.recv().expect("the platform thread answers") {
        std::panic::resume_unwind(panic);
    }
}

/// What a ratio may lose to Skia's own rounding between gradient stops: the margin the
/// resolved faces keep on every pair for exactly this (ADR-0122 decision 4).
const ROUNDING: f64 = rbs_ui::tokens::FACE_MARGIN;

/// A control of the probe: how it takes the focus, and whether its ring sits outside
/// its shape (a SplitButton part).
struct Control {
    name: &'static str,
    focus: fn(&EffectsProbe),
    outside: bool,
    /// It is in the row that opts in to the halo, which moves in Glow.
    opted_in: bool,
    /// A primary has no ring pair on its own face (`color.border.focus` on `color.accent`
    /// is about 1:1 in calm, ADR-0122 decision 5), so only the outer side is ground.
    primary: bool,
}

const CONTROLS: [Control; 13] = [
    control("mid", EffectsProbe::invoke_focus_mid, false, false),
    primary(control("go", EffectsProbe::invoke_focus_go, true, false)),
    primary(control(
        "go menu",
        EffectsProbe::invoke_focus_go_menu,
        true,
        false,
    )),
    control("record", EffectsProbe::invoke_focus_record, true, false),
    control(
        "record menu",
        EffectsProbe::invoke_focus_record_menu,
        true,
        false,
    ),
    control("icon", EffectsProbe::invoke_focus_icon, false, false),
    primary(control(
        "above",
        EffectsProbe::invoke_focus_above,
        false,
        false,
    )),
    control("below", EffectsProbe::invoke_focus_below, false, false),
    control("checked", EffectsProbe::invoke_focus_checked, false, false),
    control("danger", EffectsProbe::invoke_focus_danger, false, false),
    control("dense", EffectsProbe::invoke_focus_dense, false, false),
    control("beside", EffectsProbe::invoke_focus_beside, false, true),
    primary(control(
        "halo",
        EffectsProbe::invoke_focus_halo,
        false,
        true,
    )),
];

const fn control(
    name: &'static str,
    focus: fn(&EffectsProbe),
    outside: bool,
    opted_in: bool,
) -> Control {
    Control {
        name,
        focus,
        outside,
        opted_in,
        primary: false,
    }
}

const fn primary(mut c: Control) -> Control {
    c.primary = true;
    c
}

/// A rendered window: RGBA pixels at scale 1.
struct Shot {
    px: Vec<[u8; 4]>,
    width: usize,
}

impl Shot {
    fn of(window: &slint::Window) -> Shot {
        Shot::top(window, usize::MAX)
    }

    /// The window's top `rows` rows.
    fn top(window: &slint::Window, rows: usize) -> Shot {
        slint::platform::update_timers_and_animations();
        let shot = window.take_snapshot().expect("a snapshot");
        let width = shot.width() as usize;
        Shot {
            width,
            px: shot
                .as_slice()
                .iter()
                .take(width.saturating_mul(rows))
                .map(|px| [px.r, px.g, px.b, px.a])
                .collect(),
        }
    }

    fn height(&self) -> usize {
        self.px.len() / self.width
    }

    fn at(&self, x: usize, y: usize) -> Color {
        rgb(self.px[y * self.width + x])
    }
}

fn rgb(px: [u8; 4]) -> Color {
    Color {
        r: px[0],
        g: px[1],
        b: px[2],
        a: 255,
    }
}

/// The resolved theme of `base` with `effects` at `density`.
fn theme(file: &TokenFile, base: Base, effects: Effects, density: Density) -> ResolvedTheme {
    let mut body = ThemeBody::new(base);
    body.set_effects(effects);
    let mut theme = file.resolve(&body).expect("the theme resolves");
    assert_eq!(theme.effects, effects);
    apply_density(&mut theme.length, density, 1);
    theme
}

/// The probe with `slot` in both theme slots, shown.
fn probe(file: &TokenFile, s: &ThemeSlot) -> EffectsProbe {
    let p = EffectsProbe::new().expect("the probe window");
    apply_tokens(
        &p.global::<Theme>(),
        &p.global::<Tokens>(),
        file,
        [s.clone(), s.clone()],
    )
    .expect("the tokens apply");
    p.show().expect("show");
    p
}

/// The probe rendered with `slot`: with nothing focused, then with each control of
/// `CONTROLS` given its ring.
fn renders(file: &TokenFile, s: &ThemeSlot) -> (Shot, Vec<Shot>) {
    let p = probe(file, s);
    let plain = Shot::of(p.window());
    let focused = CONTROLS
        .iter()
        .map(|c| {
            (c.focus)(&p);
            Shot::of(p.window())
        })
        .collect();
    p.hide().expect("hide");
    (plain, focused)
}

/// The pixels that turn exactly the ring colour with the focus.
fn ring_mask(focused: &Shot, plain: &Shot, ring: [u8; 4]) -> Vec<bool> {
    focused
        .px
        .iter()
        .zip(&plain.px)
        .map(|(f, p)| *f == ring && f != p)
        .collect()
}

/// The top left corner of the mask's bounding box.
fn corner(mask: &[bool], width: usize) -> (isize, isize) {
    let xs = (0..mask.len()).filter(|i| mask[*i]).map(|i| i % width);
    let ys = (0..mask.len()).filter(|i| mask[*i]).map(|i| i / width);
    (
        xs.min().unwrap_or(0) as isize,
        ys.min().unwrap_or(0) as isize,
    )
}

/// The ground beside a ring in the calm render: every flat pixel at Chebyshev distance
/// exactly 2 from the ring, with whether the ring encloses it (it is not reachable from the image's edge without
/// crossing the ring: the control's side).
fn ground_beside(calm: &Shot, ring: &[bool]) -> Vec<(usize, bool)> {
    let width = calm.width;
    let height = ring.len() / width;
    // Only the ring's bounding box and three pixels round it matter: past the wall and
    // the ground, every pixel is outside.
    let (mut x0, mut y0, mut x1, mut y1) = (width, height, 0, 0);
    for i in (0..ring.len()).filter(|i| ring[*i]) {
        let (x, y) = (i % width, i / width);
        (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
    }
    let (x0, y0) = (x0.saturating_sub(3), y0.saturating_sub(3));
    let (x1, y1) = ((x1 + 3).min(width - 1), (y1 + 3).min(height - 1));
    let inside = |i: usize| {
        let (x, y) = (i % width, i / width);
        x >= x0 && x <= x1 && y >= y0 && y <= y1
    };
    let area: Vec<usize> = (0..ring.len()).filter(|i| inside(*i)).collect();
    let near = |i: usize, reach: isize| {
        let (x, y) = ((i % width) as isize, (i / width) as isize);
        (-reach..=reach).any(|dy| {
            (-reach..=reach).any(|dx| {
                let (nx, ny) = (x + dx, y + dy);
                nx >= 0
                    && ny >= 0
                    && (nx as usize) < width
                    && (ny as usize) < height
                    && ring[ny as usize * width + nx as usize]
            })
        })
    };
    // The ring with its anti-aliased fringe is the wall: a round ring's full pixels
    // alone leave diagonal gaps.
    let mut wall = vec![false; ring.len()];
    for i in &area {
        wall[*i] = near(*i, 1);
    }
    let mut outside = vec![false; ring.len()];
    let mut stack: Vec<usize> = area
        .iter()
        .copied()
        .filter(|i| {
            let (x, y) = (i % width, i / width);
            x == x0 || y == y0 || x == x1 || y == y1
        })
        .filter(|i| !wall[*i])
        .collect();
    while let Some(i) = stack.pop() {
        if outside[i] || wall[i] {
            continue;
        }
        outside[i] = true;
        let (x, y) = (i % width, i / width);
        if x > x0 {
            stack.push(i - 1);
        }
        if x < x1 {
            stack.push(i + 1);
        }
        if y > y0 {
            stack.push(i - width);
        }
        if y < y1 {
            stack.push(i + width);
        }
    }
    // Ground is flat: a pixel that differs from a neighbour past the ring's fringe is
    // an anti-aliased edge (a shape's rounded corner, a glyph), a blend and not ground.
    let flat = |i: usize| {
        let (x, y) = (i % width, i / width);
        let mut around = Vec::new();
        if x > 0 {
            around.push(i - 1);
        }
        if x + 1 < width {
            around.push(i + 1);
        }
        if y > 0 {
            around.push(i - width);
        }
        if y + 1 < height {
            around.push(i + width);
        }
        around
            .into_iter()
            .filter(|n| !wall[*n])
            .all(|n| calm.px[n] == calm.px[i])
    };
    area.iter()
        .copied()
        .filter(|i| !wall[*i] && near(*i, 2) && flat(*i))
        .map(|i| (i, !outside[i]))
        .collect()
}

/// A box in a window: x, y, width, height.
type Box4 = (f32, f32, f32, f32);

/// The probe's buttons by label, from its `boxes` property (the generated code carries
/// no element names, so the testing backend's element queries find nothing).
fn probe_boxes(p: &EffectsProbe) -> Vec<(String, Box4)> {
    p.get_boxes()
        .split(';')
        .filter(|b| !b.is_empty())
        .map(|b| {
            let f: Vec<&str> = b.split('|').collect();
            let n = |i: usize| f[i].parse::<f32>().expect("a number");
            (f[0].to_string(), (n(1), n(2), n(3), n(4)))
        })
        .collect()
}

/// The frame bar's controls in a render with every effect off: the bounding box of each
/// connected run of pixels that are not `ground` in the bar's rows, as tall as a button.
fn bar_boxes(shot: &Shot, ground: Color, rows: usize, min_height: f32) -> Vec<Box4> {
    let width = shot.width;
    let mut seen = vec![false; width * rows];
    let mut boxes = Vec::new();
    for start in 0..width * rows {
        if seen[start] || rgb(shot.px[start]) == ground {
            continue;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        let mut stack = vec![start];
        while let Some(i) = stack.pop() {
            if seen[i] || rgb(shot.px[i]) == ground {
                continue;
            }
            seen[i] = true;
            let (x, y) = (i % width, i / width);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            if x > 0 {
                stack.push(i - 1);
            }
            if x + 1 < width {
                stack.push(i + 1);
            }
            if y > 0 {
                stack.push(i - width);
            }
            if y + 1 < rows {
                stack.push(i + width);
            }
        }
        let b = (
            x0 as f32,
            y0 as f32,
            (x1 - x0 + 1) as f32,
            (y1 - y0 + 1) as f32,
        );
        if b.3 >= min_height {
            boxes.push(b);
        }
    }
    boxes.sort_by(|a, b| a.0.total_cmp(&b.0));
    boxes
}

/// The boxes of every button in a window and the slot's extent: where a neighbour's own
/// effects may lie.
struct Others {
    boxes: Vec<Box4>,
    extent: f32,
}

impl Others {
    fn of(boxes: Vec<Box4>, theme: &ResolvedTheme) -> Others {
        assert!(boxes.len() >= 3, "the window's buttons were not found");
        Others {
            boxes,
            extent: rbs_ui::tokens::extent(&theme.length),
        }
    }

    /// The buttons other than the ringed one: the ringed control's boxes are the ones
    /// the ring's bounding box covers.
    fn beside(&self, ring: &[usize], width: usize) -> Vec<Box4> {
        let xs = || ring.iter().map(|i| (i % width) as f32);
        let ys = || ring.iter().map(|i| (i / width) as f32);
        let (rx0, rx1) = (xs().fold(f32::MAX, f32::min), xs().fold(0.0, f32::max));
        let (ry0, ry1) = (ys().fold(f32::MAX, f32::min), ys().fold(0.0, f32::max));
        self.boxes
            .iter()
            .copied()
            .filter(|(bx, by, bw, bh)| !(*bx < rx1 && bx + bw > rx0 && *by < ry1 && by + bh > ry0))
            .collect()
    }

    /// True when the pixel lies within the extent of one of `boxes`.
    fn reach(&self, x: usize, y: usize, boxes: &[Box4]) -> bool {
        let (x, y, e) = (x as f32, y as f32, self.extent);
        boxes.iter().any(|(bx, by, bw, bh)| {
            x + 1.0 > bx - e && x < bx + bw + e && y + 1.0 > by - e && y < by + bh + e
        })
    }
}

/// A control's ring in the calm render, found once and held against each lively render:
/// its pixels, the ground beside it, and the other buttons' boxes.
struct Ring {
    pixels: Vec<usize>,
    /// Each ground pixel beside the ring, with whether the ring encloses it.
    ground: Vec<(usize, bool)>,
    width: usize,
}

impl Ring {
    /// `calm` is the render with the control focused, `calm_plain` with nothing focused.
    /// None when the control shows no ring in calm (its ring colour is its fill), where
    /// there is nothing to keep.
    fn find(calm: &Shot, calm_plain: &Shot, ring: [u8; 4], others: &Others) -> Option<Ring> {
        let width = calm.width;
        let mask = ring_mask(calm, calm_plain, ring);
        let pixels: Vec<usize> = (0..mask.len()).filter(|i| mask[*i]).collect();
        // A ring runs round its control. Where the ring colour is the control's own fill
        // (a primary in Light), only the lip's one row changes with the focus: no ring.
        let rows = pixels
            .iter()
            .map(|i| i / width)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        if pixels.len() < 40 || rows < 8 {
            return None;
        }
        let neighbours = others.beside(&pixels, width);
        let ground = ground_beside(calm, &mask)
            .into_iter()
            .filter(|(i, enclosed)| *enclosed || !others.reach(i % width, i / width, &neighbours))
            .collect();
        Some(Ring {
            pixels,
            ground,
            width,
        })
    }

    /// The top left corner of the ring's bounding box.
    fn corner(&self) -> (isize, isize) {
        let xs = self.pixels.iter().map(|i| i % self.width);
        let ys = self.pixels.iter().map(|i| i / self.width);
        (
            xs.min().unwrap_or(0) as isize,
            ys.min().unwrap_or(0) as isize,
        )
    }

    /// The ring rule: `calm` and `lively` are the renders with the control focused,
    /// `lively_plain` the lively one with nothing focused. `inner` and `outer` say which
    /// sides of the ring are ground: the control's side, and the side away from it.
    fn holds(
        &self,
        what: &str,
        ring: [u8; 4],
        calm: &Shot,
        (lively, lively_plain): (&Shot, &Shot),
        (inner, outer): (bool, bool),
        may_move: bool,
    ) {
        let width = self.width;
        // The lively ring is the calm ring, moved only where the layout keeps a clear
        // zone: there its place is read from the lively render (Glow draws no gradient
        // that could pass through the ring colour).
        let (dx, dy) = if may_move {
            let lively_ring = ring_mask(lively, lively_plain, ring);
            let (cx, cy) = self.corner();
            let (lx, ly) = corner(&lively_ring, lively.width);
            (lx - cx, ly - cy)
        } else {
            (0, 0)
        };
        let moved = |i: usize| {
            let (x, y) = ((i % width) as isize + dx, (i / width) as isize + dy);
            assert!(
                x >= 0 && y >= 0 && (x as usize) < lively.width && (y as usize) < lively.height(),
                "{what}: the ring leaves the window"
            );
            y as usize * lively.width + x as usize
        };
        let covered = self
            .pixels
            .iter()
            .filter(|i| lively.px[moved(**i)] != ring)
            .count();
        assert_eq!(
            covered,
            0,
            "{what}: {covered} of {} ring pixels differ",
            self.pixels.len()
        );
        let ring_colour = rgb(ring);
        let mut checked = 0;
        for (i, enclosed) in self.ground.iter().copied() {
            if (enclosed && !inner) || (!enclosed && !outer) {
                continue;
            }
            checked += 1;
            let was = ring_colour.contrast(rgb(calm.px[i]));
            let is = ring_colour.contrast(rgb(lively.px[moved(i)]));
            assert!(
                is + ROUNDING >= was.min(3.0),
                "{what}: beside the ring at {},{} the contrast is {is:.2}, in calm {was:.2}",
                i % width,
                i / width
            );
        }
        assert!(
            checked >= 20 || !(inner || outer),
            "{what}: only {checked} ground pixels beside the ring were found"
        );
    }
}

/// A slot of `theme` drawn as calm that keeps the halo's clear zone of Glow: the
/// reference render in which only the layout shows what lies round a primary.
fn zone_only(file: &TokenFile, base: Base, density: Density) -> ThemeSlot {
    let glow = slot(&theme(file, base, Effects::Glow, density), "").expect("a slot");
    let mut calm = slot(&theme(file, base, Effects::Calm, density), "").expect("a slot");
    assert!(glow.halo_clear > 0.0);
    calm.halo_clear = glow.halo_clear;
    calm
}

/// Every pixel within `zone` of the box and outside it equals `ground`.
fn zone_is_clear(
    what: &str,
    shot: &Shot,
    (x, y, w, h): (f32, f32, f32, f32),
    zone: f32,
    ground: Color,
) {
    let (x0, y0) = ((x - zone).floor() as isize, (y - zone).floor() as isize);
    let (x1, y1) = (
        (x + w + zone).ceil() as isize,
        (y + h + zone).ceil() as isize,
    );
    assert!(
        x0 >= 0 && y0 >= 0 && (x1 as usize) <= shot.width && (y1 as usize) <= shot.height(),
        "{what}: the clear zone {x0},{y0} to {x1},{y1} leaves the window"
    );
    for py in y0..y1 {
        for px in x0..x1 {
            let (fx, fy) = (px as f32, py as f32);
            if fx >= x.floor() && fx < (x + w).ceil() && fy >= y.floor() && fy < (y + h).ceil() {
                continue;
            }
            let c = shot.at(px as usize, py as usize);
            assert_eq!(
                c, ground,
                "{what}: {c} at {px},{py} lies in the clear zone of the box {x},{y} {w}x{h}"
            );
        }
    }
}

/// Regular density. Over 1 s (ADR-0032 decision 5): each of the about 150 renders of a
/// real window takes some 15 ms in a debug build, and every setting, both bases and
/// every control need their own render.
#[test]
fn effects_keep_every_focus_ring_every_size_and_the_halo_zone() {
    on_the_platform_thread(|| {
        let file = TokenFile::built_in().unwrap();
        effects_change_the_layout_only_at_opted_in_positions(&file);
        every_ring_keeps_its_contrast(&file, Density::Regular);
        faces_keep_their_text_pairs(&file);
        the_probe_keeps_the_halo_zone_clear(&file);
        the_frame_bar_keeps_its_layout_and_its_neighbours_rings(&file, Density::Regular);
        the_caption_row_follows_the_frame_bar(&file);
        the_chosen_plate_keeps_its_ring_and_its_track(&file);
    });
}

/// The same rings and the frame bar at compact density, where the spacing is tightest:
/// as many renders again.
#[test]
#[ignore = "slow: about 150 more renders of real windows, some 15 ms each in a debug build"]
fn slow_effects_keep_every_focus_ring_at_compact_density() {
    on_the_platform_thread(|| {
        let file = TokenFile::built_in().unwrap();
        every_ring_keeps_its_contrast(&file, Density::Compact);
        the_frame_bar_keeps_its_layout_and_its_neighbours_rings(&file, Density::Compact);
    });
}

fn every_ring_keeps_its_contrast(file: &TokenFile, density: Density) {
    for base in [Base::Dark, Base::Light] {
        {
            let slot_of = |effects| slot(&theme(file, base, effects, density), "").expect("a slot");
            let calm = slot_of(Effects::Calm);
            let ring = file.built_in_theme(base).colors["color.border.focus"];
            let ring = [ring.r, ring.g, ring.b, 255];
            let (calm_plain, calm_focused) = renders(file, &calm);
            let others = {
                let p = probe(file, &calm);
                // A render first, so every place is laid out.
                let _ = Shot::of(p.window());
                let boxes = probe_boxes(&p).into_iter().map(|(_, b)| b).collect();
                let o = Others::of(boxes, &theme(file, base, Effects::Calm, density));
                p.hide().unwrap();
                o
            };
            let rings: Vec<Option<Ring>> = calm_focused
                .iter()
                .map(|shot| Ring::find(shot, &calm_plain, ring, &others))
                .collect();
            let mut ringed = 0;
            for effects in Effects::LIVELY {
                let (lively_plain, lively_focused) = renders(file, &slot_of(effects));
                for (n, c) in CONTROLS.iter().enumerate() {
                    let Some(found) = &rings[n] else {
                        continue;
                    };
                    found.holds(
                        &format!("{} with {effects:?} ({base:?}, {density:?})", c.name),
                        ring,
                        &calm_focused[n],
                        (&lively_focused[n], &lively_plain),
                        (!c.primary, c.outside),
                        // Only the opted-in row moves, and only in Glow.
                        c.opted_in && effects == Effects::Glow,
                    );
                    ringed += 1;
                }
            }
            // In Light the ring colour is the accent, so a primary shows no ring of its
            // own in calm (ADR-0118); every other control does.
            let primaries = if base == Base::Light { 2 } else { 0 };
            let expected = (CONTROLS.len() - primaries) * Effects::LIVELY.len();
            assert!(
                ringed >= expected,
                "{ringed} of {expected} rings were checked ({base:?}, {density:?})"
            );
        }
    }
}

/// Effects never change a control's size or place (ADR-0120 decision 11), except at a
/// position that opts in to the halo, which grows by exactly the clear zone in Glow
/// (ADR-0122 decision 10).
fn effects_change_the_layout_only_at_opted_in_positions(file: &TokenFile) {
    for density in [Density::Regular, Density::Compact, Density::Roomy] {
        let space_2 = theme(file, Base::Light, Effects::Calm, density).length["space.2"];
        let geometry = |effects| {
            let s = slot(&theme(file, Base::Light, effects, density), "").unwrap();
            let p = probe(file, &s);
            let g = (p.get_geometry(), p.get_halo_left(), p.get_halo_gap());
            p.hide().unwrap();
            (g, s.halo_clear)
        };
        let (calm, clear) = geometry(Effects::Calm);
        assert_eq!(clear, 0.0);
        assert_eq!((calm.1, calm.2), (0.0, space_2), "{density:?}");
        for e in Effects::LIVELY {
            let (g, clear) = geometry(e);
            assert_eq!(g.0, calm.0, "{e:?} {density:?}");
            if e == Effects::Glow {
                assert_eq!(clear, 13.0);
                assert_eq!((g.1, g.2), (clear, clear.max(space_2)), "{density:?}");
            } else {
                assert_eq!((g.1, g.2), (calm.1, calm.2), "{e:?} {density:?}");
            }
        }
    }
}

/// The opted-in row of the probe: with the primary's effects off and the zone kept,
/// nothing but the ground lies within `halo-clear` of the primary's box.
fn the_probe_keeps_the_halo_zone_clear(file: &TokenFile) {
    for base in [Base::Dark, Base::Light] {
        for density in [Density::Regular, Density::Compact] {
            let s = zone_only(file, base, density);
            let p = probe(file, &s);
            let shot = Shot::of(p.window());
            let b = (
                p.get_halo_x(),
                p.get_halo_y(),
                p.get_halo_w(),
                p.get_halo_h(),
            );
            p.hide().unwrap();
            zone_is_clear(
                &format!("the probe ({base:?}, {density:?})"),
                &shot,
                b,
                s.halo_clear,
                file.built_in_theme(base).colors["color.surface.base"],
            );
        }
    }
}

/// The contrast rule minimum for `fg` drawn on `bg`.
fn rule(fg: &str, bg: &str) -> Option<f64> {
    CONTRAST_RULES
        .iter()
        .chain(THEME_CONTRAST_RULES)
        .find(|(f, b, ..)| *f == fg && *b == bg)
        .map(|(_, _, min)| *min)
}

/// Text is not measured from anti-aliased pixels: the face colour is sampled at the ends
/// of the ink band, in the padding next to the label, and held against the kind's known
/// ink colours (ADR-0122 decision 10).
fn faces_keep_their_text_pairs(file: &TokenFile) {
    let buttons = [
        ("Left", KIND_PRIMARY),
        ("Focused", KIND_SECONDARY),
        ("Checked", KIND_CHECKED),
        ("Delete", KIND_DANGER),
        ("Dense", KIND_SECONDARY),
        ("Main action", KIND_PRIMARY),
    ];
    for base in [Base::Dark, Base::Light] {
        for density in [Density::Regular, Density::Compact] {
            for effects in Effects::LIVELY {
                let t = theme(file, base, effects, density);
                let line = f32::from(t.type_styles["type.label"].line);
                let pad = t.length["space.3"];
                let p = probe(file, &slot(&t, "").unwrap());
                let shot = Shot::of(p.window());
                let boxes = probe_boxes(&p);
                for (label, kind) in buttons {
                    let (bx, by, _, bh) = boxes
                        .iter()
                        .find(|(l, _)| l == label)
                        .unwrap_or_else(|| panic!("no button {label}"))
                        .1;
                    let band = band_top(bh, line);
                    // In the side padding, clear of the border and the label.
                    let x = (bx + pad / 2.0).round() as usize;
                    let rows = [
                        (by + band).round() as usize,
                        (by + bh - band).round() as usize - 1,
                    ];
                    let kind = &FACE_KINDS[kind];
                    let fill = t.colors[kind.fills[0]];
                    for y in rows {
                        let face = shot.at(x, y);
                        for ink in kind.inks {
                            let Some(need) = rule(ink, kind.fills[0]) else {
                                continue;
                            };
                            let ink_colour = t.colors[*ink];
                            let (is, calm) = (ink_colour.contrast(face), ink_colour.contrast(fill));
                            // A pair under its rule in calm is ADR-0072's to warn about.
                            if calm < need {
                                continue;
                            }
                            assert!(
                                is + ROUNDING >= (need + ROUNDING).min(calm),
                                "{label} with {effects:?} ({base:?}, {density:?}): {ink} on the face {face} at row {y} has contrast {is:.2}, needs {need} (calm {calm:.2})"
                            );
                        }
                    }
                }
                p.hide().unwrap();
            }
        }
    }
}

/// The rows of the main window the frame bar tests read: the bar, and the rows below it
/// that a ring could reach.
const BAR_ROWS: usize = 96;

/// The main window with a hand-made toolbar: one item before Go live, then Go live and
/// Record, as plain Buttons or (`vertical`) as SplitButtons with the ▾ part.
fn main_window(file: &TokenFile, s: &ThemeSlot, vertical: bool) -> MainWindow {
    let item = |id: &str, title: &str| ToolbarData {
        id: id.into(),
        title: title.into(),
        kind: 0,
        enabled: true,
        checked: false,
        live: false,
        icon: "".into(),
        icon_only: false,
    };
    let model = |v: Vec<ToolbarData>| slint::ModelRc::new(slint::VecModel::from(v));
    let w = MainWindow::new().expect("the main window");
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [s.clone(), s.clone()],
    )
    .expect("the tokens apply");
    let shell = w.global::<Shell>();
    shell.set_toolbar(model(Vec::new()));
    shell.set_toolbar_end(model(vec![item("studio.mode", "Studio mode")]));
    shell.set_toolbar_fixed(model(vec![
        item("stream.toggle", "Go live"),
        item("record.toggle", "Record"),
    ]));
    shell.set_toolbar_visible(true);
    shell.set_vertical_outputs(vertical);
    w.window().set_size(slint::LogicalSize::new(1280.0, 720.0));
    w.show().expect("show");
    w
}

/// The frame bar's controls in a zone-only render, and Go live's whole box among them:
/// the one filled with the accent (as a SplitButton, its command and ▾ parts together).
fn bar_controls(shot: &Shot, t: &ResolvedTheme, clear: f32) -> (Vec<Box4>, Box4) {
    let (md, ring) = (t.length["size.control.md"], t.length["size.focus-ring"]);
    let rows = (md + 2.0 * (clear + ring)) as usize;
    let boxes = bar_boxes(shot, t.colors["color.surface.base"], rows, md - 2.0);
    let accent = t.colors[FACE_KINDS[KIND_PRIMARY].fills[0]];
    let go: Vec<Box4> = boxes
        .iter()
        .copied()
        .filter(|(x, y, w, h)| {
            shot.at((x + w / 2.0) as usize, (y + h - 3.0) as usize) == accent
                || shot.at((x + 3.0) as usize, (y + h / 2.0) as usize) == accent
        })
        .collect();
    assert_eq!(go.len(), 1, "one accent button on the frame bar: {boxes:?}");
    (boxes, go[0])
}

fn tab(w: &MainWindow) {
    let text: slint::SharedString = slint::platform::Key::Tab.into();
    w.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    w.window().dispatch_event(WindowEvent::KeyReleased { text });
}

/// The real frame bar (ADR-0136), as plain Buttons and as SplitButtons: it does not opt
/// in to the halo, so in every setting it keeps Calm's layout and nothing of an effect
/// lies beyond the extent of a control's box, and each neighbour of Go live keeps its
/// focus ring's contrast beside Go live's shadow and Record's own effects.
fn the_frame_bar_keeps_its_layout_and_its_neighbours_rings(file: &TokenFile, density: Density) {
    for base in [Base::Dark, Base::Light] {
        {
            for vertical in [false, true] {
                let what = format!("the frame bar ({base:?}, {density:?}, vertical {vertical})");
                let colours = file.built_in_theme(base).colors;
                let ring = colours["color.border.focus"];
                let ring = [ring.r, ring.g, ring.b, 255];
                // 1. The layout: every pixel beyond the extent of the controls' boxes is
                // the same as in Calm, in every lively setting. So the bar keeps its
                // height and its items their places, and Go live draws no halo.
                let calm_theme = theme(file, base, Effects::Calm, density);
                let w = main_window(file, &slot(&calm_theme, "").unwrap(), vertical);
                let calm_plain = Shot::top(w.window(), BAR_ROWS);
                let (boxes, go) =
                    bar_controls(&calm_plain, &calm_theme, calm_theme.length["space.1"]);
                assert_eq!(
                    go.3, calm_theme.length["size.control.md"],
                    "{what}: Go live's height"
                );
                let others = Others::of(boxes.clone(), &calm_theme);
                // The bar and its bottom border; the panels below have their own buttons.
                let l = &calm_theme.length;
                let bar =
                    (l["size.control.md"] + 2.0 * l["space.1"] + l["size.border"]).ceil() as usize;
                for e in Effects::LIVELY {
                    let s = slot(&theme(file, base, e, density), "").unwrap();
                    assert_eq!(s.halo_clear > 0.0, e == Effects::Glow);
                    let lively = main_window(file, &s, vertical);
                    let shot = Shot::top(lively.window(), BAR_ROWS);
                    assert_ne!(calm_plain.px, shot.px, "{what}: {e:?} draws nothing");
                    for y in 0..bar {
                        for x in 0..shot.width {
                            if others.reach(x, y, &boxes) {
                                continue;
                            }
                            assert_eq!(
                                shot.at(x, y),
                                calm_plain.at(x, y),
                                "{what}: {e:?} changes {x},{y}, beyond the extent of every control"
                            );
                        }
                    }
                    lively.hide().unwrap();
                }
                let glow = slot(&theme(file, base, Effects::Glow, density), "").unwrap();
                let lit = main_window(file, &glow, vertical);
                let lively_plain = Shot::top(lit.window(), BAR_ROWS);
                // 2. The neighbours' rings, in Calm and in Glow: Tab moves the focus
                // along the row. The neighbours are the bar's controls right beside Go
                // live.
                let at = boxes.iter().position(|b| *b == go).unwrap();
                let wanted: Vec<Box4> = [at.checked_sub(1), Some(at + 1)]
                    .into_iter()
                    .flatten()
                    .filter_map(|i| boxes.get(i).copied())
                    .collect();
                assert_eq!(
                    wanted.len(),
                    2,
                    "{what}: a control on each side of Go live: {boxes:?}"
                );
                // A SplitButton's two parts each take the focus.
                let rings = if vertical { 3 } else { 2 };
                let mut seen = 0;
                // The focus wraps round the window: each ring is checked once.
                let mut corners = Vec::new();
                for _ in 0..16 {
                    if seen == rings {
                        break;
                    }
                    tab(&w);
                    tab(&lit);
                    let calm = Shot::top(w.window(), BAR_ROWS);
                    let Some(found) = Ring::find(&calm, &calm_plain, ring, &others) else {
                        continue;
                    };
                    let (rx, ry) = found.corner();
                    if corners.contains(&(rx, ry)) {
                        continue;
                    }
                    corners.push((rx, ry));
                    let Some(side) = wanted.iter().position(|b| {
                        rx as f32 >= b.0 - 3.0
                            && (rx as f32) < b.0 + b.2
                            && (ry as f32 - b.1).abs() <= 3.0
                    }) else {
                        continue;
                    };
                    let name = ["before Go live", "after Go live"][side];
                    found.holds(
                        &format!("the control {name} on {what}"),
                        ring,
                        &calm,
                        (&Shot::top(lit.window(), BAR_ROWS), &lively_plain),
                        // The control after Go live is Record, a SplitButton here.
                        (true, vertical && side == 1),
                        false,
                    );
                    seen += 1;
                }
                assert_eq!(seen, rings, "{what}: the rings that were checked");
                w.hide().unwrap();
                lit.hide().unwrap();
            }
        }
    }
}

/// On Windows the caption row reported for the hit test is the frame bar's height
/// (ADR-0071). It is the same in Calm and Glow (ADR-0136), and a switch of density, which
/// changes `size.control.md`, publishes the new height.
fn the_caption_row_follows_the_frame_bar(file: &TokenFile) {
    let row = |t: &ResolvedTheme| {
        t.length["size.control.md"] + 2.0 * t.length["space.1"] + t.length["size.border"]
    };
    let regular = theme(file, Base::Dark, Effects::Calm, Density::Regular);
    let compact = theme(file, Base::Dark, Effects::Glow, Density::Compact);
    assert_ne!(row(&regular), row(&compact));
    let w = main_window(file, &slot(&regular, "").unwrap(), true);
    let published = Rc::new(RefCell::new(Vec::new()));
    let shell = w.global::<Shell>();
    shell.on_caption_regions({
        let published = published.clone();
        move |_, _, _, _, height| published.borrow_mut().push(height)
    });
    shell.set_custom_caption(true);
    // The narrowest window, so the regions' places do not move with the switch.
    w.window().set_size(slint::LogicalSize::new(1280.0, 720.0));
    let _ = Shot::of(w.window());
    assert_eq!(
        published.borrow().last().copied(),
        Some(row(&regular)),
        "the caption row in Calm"
    );
    let switch = |t: &ResolvedTheme| {
        let s = slot(t, "").unwrap();
        apply_tokens(
            &w.global::<Theme>(),
            &w.global::<Tokens>(),
            file,
            [s.clone(), s],
        )
        .unwrap();
        let _ = Shot::of(w.window());
    };
    switch(&theme(file, Base::Dark, Effects::Glow, Density::Regular));
    assert_eq!(
        published.borrow().last().copied(),
        Some(row(&regular)),
        "the caption row in Glow"
    );
    switch(&compact);
    assert_eq!(
        published.borrow().last().copied(),
        Some(row(&compact)),
        "the caption row at compact density"
    );
    w.hide().unwrap();
}

/// The segment probe with `slot`, shown; with `keyed`, the chosen "Regular" segment of
/// the first control has the keyboard focus (two Tabs: "Roomy", then "Regular").
fn segments(file: &TokenFile, s: &ThemeSlot, keyed: bool) -> (Shot, String) {
    let p = SegmentProbe::new().expect("the segment probe");
    apply_tokens(
        &p.global::<Theme>(),
        &p.global::<Tokens>(),
        file,
        [s.clone(), s.clone()],
    )
    .expect("the tokens apply");
    p.show().expect("show");
    let _ = Shot::of(p.window());
    if keyed {
        for _ in 0..2 {
            let tab: slint::SharedString = slint::platform::Key::Tab.into();
            p.window()
                .dispatch_event(WindowEvent::KeyPressed { text: tab.clone() });
            p.window()
                .dispatch_event(WindowEvent::KeyReleased { text: tab });
        }
    }
    let shot = Shot::of(p.window());
    let boxes = p.get_boxes().to_string();
    p.hide().expect("hide");
    (shot, boxes)
}

/// A SegmentedControl's chosen plate (ADR-0120 decision 14): in every setting the
/// controls keep their places and sizes, nothing of an effect is drawn outside a
/// control's track, and the focused plate's ring is drawn exactly as in calm, with
/// at least min(3, its calm ratio) against the plate two pixels inside it.
fn the_chosen_plate_keeps_its_ring_and_its_track(file: &TokenFile) {
    for base in [Base::Dark, Base::Light] {
        let slot_of =
            |effects| slot(&theme(file, base, effects, Density::Regular), "").expect("a slot");
        let colour = file.built_in_theme(base).colors["color.border.focus"];
        let ring_px = [colour.r, colour.g, colour.b, 255];
        let (calm_plain, calm_boxes) = segments(file, &slot_of(Effects::Calm), false);
        let (calm_keyed, _) = segments(file, &slot_of(Effects::Calm), true);
        let calm_ring = ring_mask(&calm_keyed, &calm_plain, ring_px);
        assert!(
            calm_ring.iter().any(|r| *r),
            "{base:?}: the ring shows in calm"
        );
        let tracks: Vec<[usize; 4]> = calm_boxes
            .split(';')
            .filter(|b| !b.is_empty())
            .map(|b| {
                let v: Vec<usize> = b
                    .split('|')
                    .skip(1)
                    .map(|n| n.parse::<f32>().unwrap() as usize)
                    .collect();
                [v[0], v[1], v[2], v[3]]
            })
            .collect();
        let in_track = |x: usize, y: usize| {
            tracks
                .iter()
                .any(|[tx, ty, w, h]| x >= *tx && x < tx + w && y >= *ty && y < ty + h)
        };
        // The ring's lowest row, in its middle: the plate two pixels above it.
        let ys = (0..calm_ring.len())
            .filter(|i| calm_ring[*i])
            .map(|i| i / calm_plain.width);
        let bottom = ys.max().unwrap();
        let xs: Vec<usize> = (0..calm_plain.width)
            .filter(|x| calm_ring[bottom * calm_plain.width + x])
            .collect();
        let mid = xs[xs.len() / 2];
        let calm_ratio = colour.contrast(calm_keyed.at(mid, bottom - 2));
        for effects in Effects::LIVELY {
            let why = format!("{base:?} {effects:?}");
            let (plain, boxes) = segments(file, &slot_of(effects), false);
            assert_eq!(boxes, calm_boxes, "{why}: the controls keep their places");
            // The Button row below is the buttons' own business: only the tracks' rows.
            let last = tracks.iter().map(|[_, y, _, h]| y + h).max().unwrap();
            for y in 0..last {
                for x in 0..plain.width {
                    if !in_track(x, y) {
                        assert_eq!(
                            plain.at(x, y),
                            calm_plain.at(x, y),
                            "{why}: ({x}, {y}) outside a track"
                        );
                    }
                }
            }
            let (keyed, _) = segments(file, &slot_of(effects), true);
            assert_eq!(
                ring_mask(&keyed, &plain, ring_px),
                calm_ring,
                "{why}: the ring is whole"
            );
            let ratio = colour.contrast(keyed.at(mid, bottom - 2));
            assert!(
                ratio + ROUNDING >= calm_ratio.min(3.0),
                "{why}: the ring keeps {ratio:.2} against the plate (calm {calm_ratio:.2})"
            );
        }
    }
}
