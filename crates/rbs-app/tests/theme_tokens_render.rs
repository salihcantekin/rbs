//! Every themeable token changes what the app draws (themes spec, "Themeable tokens";
//! ADR-0057). For each token of `THEMEABLE`, the views below are rendered with Skia's
//! real pixels twice: with the built-in Dark theme and with that one token at a far
//! value. A token that changes no pixel in any view is dead: a user who edits it in
//! Settings › Appearance sees nothing happen.
//!
//! The far value bypasses the theme rules on purpose (`Tokens::resolve` would refuse,
//! for example, `radius.md` above `radius.lg`): this test asks whether the UI reads the
//! token, not whether a theme may hold the value.
//!
//! Slow (about a thousand renders), so ignored: `cargo test -p rbs-app --test
//! theme_tokens_render -- --ignored`. With `RBS_SHOTS_DIR` set it also writes the table
//! (`tokens.tsv`) and, for each token named in `RBS_DIFF_TOKENS` (comma separated), raw
//! RGBA images of each changed view: the default, the far value, and a diff mask.

//!
//! A token read only in a state these still views do not show (hover, press, focus, a
//! notice, an open popup, a meter with signal) is listed in `STATE_ONLY` with where it
//! is drawn; the fast test `every_themeable_token_is_read_by_the_ui` still checks that
//! the UI reads it.

#![allow(clippy::expect_used, clippy::unwrap_used)] // Tests fail by panicking.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{
    EffectsProbe, ExperienceProbe, MainWindow, NumberFieldProbe, PropertiesProbe, SegmentProbe,
    SettingRowProbe, SettingsWindow, Shell, Theme, ThemeSlot, Tokens, ToolbarData,
};
use rbs_ui::tokens::{Base, Color, Kind, THEMEABLE, ThemeBody, Tokens as TokenFile, TypeStyle};
use slint::ComponentHandle;

/// Tokens drawn only in a state the views do not show, with where.
const STATE_ONLY: &[(&str, &str)] = &[
    ("color.surface.checker", "behind a transparent picture"),
    ("color.scrollbar.thumb-hot", "a scrollbar under the pointer"),
    ("color.toast.surface", "a notice"),
    ("color.toast.edge", "a notice"),
    ("color.border.focus", "the keyboard focus ring"),
    (
        "color.accent.subtle.hover",
        "a selected row under the pointer",
    ),
    ("color.accent.pressed", "a primary button while pressed"),
    ("color.button.hover", "a button under the pointer"),
    ("color.button.pressed", "a button while pressed"),
    ("color.state.warning", "warnings, the paused recording"),
    ("color.state.healthy", "a healthy output"),
    (
        "color.action.destructive.subtle.hover",
        "a danger button under the pointer",
    ),
    (
        "color.action.destructive",
        "the danger rim of Glow, Glass and Tactile only",
    ),
    ("color.meter.low", "an audio meter with signal"),
    ("color.meter.mid", "an audio meter with signal"),
    ("color.meter.high", "an audio meter with signal"),
    ("color.meter.clip", "an audio meter with signal"),
    ("color.loudness.fill", "the loudness meter with signal"),
    ("color.loudness.target", "the loudness meter with signal"),
    (
        "space.8",
        "the colour picker popup, OBS import, Measure sync offset",
    ),
    (
        "size.section.gap",
        "the studio panels (Properties, Library, Audio)",
    ),
    ("size.scrollbar.thumb-hot", "a scrollbar under the pointer"),
    ("size.scrollbar.thumb-min", "a long list's scrollbar"),
    ("size.list.cell-min-w", "a list in columns"),
    ("size.select.list-h", "an open drop-down"),
    ("size.app-menu.w", "the open RBS menu"),
    ("size.app-menu.categories", "the open RBS menu"),
    ("size.app-menu.list-h", "the open RBS menu"),
    ("size.toast.border", "a notice"),
    ("motion.fast", "an animation"),
    ("motion.normal", "an animation"),
];

/// The `.slint` spelling of a token: `Theme.surface-base`, `Tokens.space-1`,
/// `Tokens.type-body-` (size, line, weight), `Theme.font-family`.
fn slint_name(name: &str) -> String {
    match name.strip_prefix("color.") {
        Some(c) => format!("Theme.{}", c.replace('.', "-")),
        None if name == "font.family" => "Theme.font-family".to_owned(),
        None if name.starts_with("type.") => format!("Tokens.{}-", name.replace('.', "-")),
        None => format!("Tokens.{}", name.replace('.', "-")),
    }
}

/// Every themeable token is read by the desktop UI, outside the test probes: a token
/// no `.slint` file reads changes nothing a user can see, however it is set. Colours
/// the app draws only through the effects (`color.action.destructive`, the danger rim)
/// are read in Rust (`rbs_ui::tokens::FACE_KINDS`) and listed here.
#[test]
fn every_themeable_token_is_read_by_the_ui() {
    const ONLY_IN_EFFECTS: &[&str] = &["color.action.destructive"];
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    let mut text = String::new();
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let probe = p
            .file_name()
            .is_some_and(|n| n.to_string_lossy().contains("probe"));
        if p.extension().is_some_and(|x| x == "slint") && !probe {
            text.push_str(&std::fs::read_to_string(&p).unwrap());
        }
    }
    let unread: Vec<&str> = THEMEABLE
        .iter()
        .map(|t| t.name)
        .filter(|n| !ONLY_IN_EFFECTS.contains(n))
        .filter(|n| {
            let s = slint_name(n);
            // The next character must end the name (`Theme.accent` is not `Theme.accent-hover`).
            !text.match_indices(&s).any(|(i, _)| {
                s.ends_with('-')
                    || !text[i + s.len()..]
                        .starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-')
            })
        })
        .collect();
    assert!(
        unread.is_empty(),
        "themeable tokens no .slint file reads: {unread:?}"
    );
}

/// A rendered view: width, height and RGBA bytes.
struct Shot {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

fn snap(w: &slint::Window) -> Shot {
    let px = w.take_snapshot().expect("a snapshot");
    Shot {
        w: px.width(),
        h: px.height(),
        px: px.as_bytes().to_vec(),
    }
}

/// Gives a view's window the slots.
type Apply = Box<dyn Fn(&TokenFile, &ThemeSlot)>;

/// A window the test renders, with how to give it the slots.
struct View {
    name: String,
    window: Box<dyn Fn() -> Shot>,
    apply: Apply,
}

macro_rules! view {
    ($name:expr, $w:expr, $width:expr, $height:expr) => {{
        let w = $w;
        w.window()
            .set_size(slint::LogicalSize::new($width as f32, $height as f32));
        w.show().expect("show");
        let a = w.clone_strong();
        let b = w.clone_strong();
        View {
            name: $name.to_string(),
            window: Box::new(move || snap(a.window())),
            apply: Box::new(move |file: &TokenFile, s: &ThemeSlot| {
                apply_tokens(
                    &b.global::<Theme>(),
                    &b.global::<Tokens>(),
                    file,
                    [s.clone(), s.clone()],
                )
                .expect("the tokens apply");
            }),
        }
    }};
}

fn views() -> Vec<View> {
    let mut v = Vec::new();
    // The main window with a toolbar (the frame bar, the docked panels' frames).
    let main = MainWindow::new().expect("the main window");
    {
        let item = |id: &str, title: &str, icon: &str| ToolbarData {
            id: id.into(),
            title: title.into(),
            kind: 0,
            enabled: true,
            checked: false,
            live: false,
            icon: icon.into(),
            icon_only: false,
        };
        let model = |v: Vec<ToolbarData>| slint::ModelRc::new(slint::VecModel::from(v));
        let shell = main.global::<Shell>();
        shell.set_toolbar(model(vec![item("scene.add", "Add scene", "plus")]));
        shell.set_toolbar_end(model(vec![item("studio.mode", "Studio mode", "")]));
        shell.set_toolbar_fixed(model(vec![
            item("stream.toggle", "Go live", ""),
            item("record.toggle", "Record", ""),
        ]));
        shell.set_toolbar_visible(true);
    }
    v.push(view!("main", main, 1280, 720));
    // Each Settings section (0 Video ... 10 AI tools).
    for section in 0..=10 {
        let w = SettingsWindow::new().expect("the settings window");
        w.set_section(section);
        v.push(view!(format!("settings-{section}"), w, 960, 1400));
    }
    v.push(view!(
        "properties",
        PropertiesProbe::new().unwrap(),
        420,
        900
    ));
    v.push(view!(
        "effects-probe",
        EffectsProbe::new().unwrap(),
        480,
        400
    ));
    v.push(view!(
        "segment-probe",
        SegmentProbe::new().unwrap(),
        480,
        200
    ));
    v.push(view!(
        "setting-row-probe",
        SettingRowProbe::new().unwrap(),
        400,
        200
    ));
    v.push(view!(
        "number-field-probe",
        NumberFieldProbe::new().unwrap(),
        400,
        200
    ));
    v.push(view!(
        "experience-probe",
        ExperienceProbe::new().unwrap(),
        400,
        300
    ));
    v
}

/// A colour far from `c`.
fn far_colour(c: Color) -> Color {
    let magenta = Color::parse("#ff00ff").unwrap();
    let cyan = Color::parse("#00ffff").unwrap();
    let d = |a: Color, b: Color| {
        (i32::from(a.r) - i32::from(b.r)).abs()
            + (i32::from(a.g) - i32::from(b.g)).abs()
            + (i32::from(a.b) - i32::from(b.b)).abs()
    };
    if d(c, magenta) > 150 { magenta } else { cyan }
}

/// The far value of a length between `min` and `max`: whichever bound is further away.
fn far_length(v: f32, min: f32, max: f32) -> f32 {
    if max - v >= v - min { max } else { min }
}

/// One variation: the token, what it was set to, and the theme with it.
fn variations(
    file: &TokenFile,
    base: &rbs_ui::tokens::ResolvedTheme,
) -> Vec<(String, String, rbs_ui::tokens::ResolvedTheme, String)> {
    let _ = file;
    let mut out = Vec::new();
    for t in THEMEABLE {
        let mut th = base.clone();
        let mut font = String::new();
        let what = match t.kind {
            Kind::Color => {
                let c = far_colour(th.colors[t.name]);
                th.colors.insert(t.name.to_owned(), c);
                format!("{c:?}")
            }
            Kind::Length { min, max } => {
                let v = far_length(th.length[t.name], min, max);
                th.length.insert(t.name.to_owned(), v);
                format!("{v}px")
            }
            Kind::Duration { .. } => "not visible in a still picture".to_owned(),
            Kind::TypeStyle => {
                let s = TypeStyle {
                    size: 24,
                    line: 32,
                    weight: 800,
                };
                th.type_styles.insert(t.name.to_owned(), s);
                "24/32/800".to_owned()
            }
            Kind::FontFamily => {
                font = "DejaVu Serif".to_owned();
                "DejaVu Serif".to_owned()
            }
        };
        out.push((t.name.to_owned(), what, th, font));
    }
    out
}

fn changed(a: &Shot, b: &Shot) -> usize {
    if (a.w, a.h) != (b.w, b.h) {
        return (a.w * a.h) as usize;
    }
    a.px.chunks_exact(4)
        .zip(b.px.chunks_exact(4))
        .filter(|(x, y)| x != y)
        .count()
}

fn write(dir: &std::path::Path, name: &str, s: &Shot) {
    std::fs::write(dir.join(format!("{name}-{}x{}.rgba", s.w, s.h)), &s.px).unwrap();
}

fn mask(a: &Shot, b: &Shot) -> Shot {
    let px =
        a.px.chunks_exact(4)
            .zip(b.px.chunks_exact(4))
            .flat_map(|(x, y)| {
                if x == y {
                    let g = x[0] / 4 + x[1] / 4 + x[2] / 4;
                    [g / 2, g / 2, g / 2, 255]
                } else {
                    [255, 0, 255, 255]
                }
            })
            .collect();
    Shot { w: a.w, h: a.h, px }
}

#[test]
#[ignore = "slow: renders every themeable token in every view"]
fn slow_every_themeable_token_changes_a_pixel() {
    let backend = i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            renderer_name: Some("skia".into()),
            ..Default::default()
        },
    );
    slint::platform::set_platform(Box::new(backend)).expect("the testing platform");
    let dir = std::env::var_os("RBS_SHOTS_DIR").map(std::path::PathBuf::from);
    if let Some(d) = &dir {
        std::fs::create_dir_all(d).unwrap();
    }
    let diff: Vec<String> = std::env::var("RBS_DIFF_TOKENS")
        .map(|s| s.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let file = TokenFile::built_in().unwrap();
    let base = file.resolve(&ThemeBody::new(Base::Dark)).unwrap();
    let views = views();
    let plain = slot(&base, "").unwrap();
    let defaults: Vec<Shot> = views
        .iter()
        .map(|v| {
            (v.apply)(&file, &plain);
            (v.window)()
        })
        .collect();
    let mut table = String::from("token\tvalue\ttotal\tviews\n");
    let mut dead = Vec::new();
    for (name, what, th, font) in variations(&file, &base) {
        let s = slot(&th, &font).unwrap();
        let mut total = 0;
        let mut hit = Vec::new();
        for (v, d) in views.iter().zip(&defaults) {
            (v.apply)(&file, &s);
            let shot = (v.window)();
            let n = changed(d, &shot);
            if n > 0 {
                total += n;
                hit.push(format!("{}:{n}", v.name));
                if let Some(dir) = &dir
                    && diff.contains(&name)
                {
                    write(dir, &format!("{name}--{}--far", v.name), &shot);
                    write(dir, &format!("{name}--{}--default", v.name), d);
                    if (d.w, d.h) == (shot.w, shot.h) {
                        write(dir, &format!("{name}--{}--mask", v.name), &mask(d, &shot));
                    }
                }
            }
            (v.apply)(&file, &plain);
        }
        table.push_str(&format!("{name}\t{what}\t{total}\t{}\n", hit.join(" ")));
        let state_only = STATE_ONLY.iter().any(|(n, _)| *n == name);
        if total == 0 && !state_only {
            dead.push(name);
        } else if total > 0 && state_only {
            // Listed as state only but seen: the list is out of date.
            dead.push(format!("{name} (seen, remove it from STATE_ONLY)"));
        }
    }
    if let Some(d) = &dir {
        std::fs::write(d.join("tokens.tsv"), &table).unwrap();
    }
    println!("{table}");
    assert!(
        dead.is_empty(),
        "tokens that change nothing in any view: {dead:?}"
    );
}
