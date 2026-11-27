//! Screenshots of the searchable picker (design system, "Searchable picker"; owner
//! 2026-11-27) for each screen that uses it, in Dark and Light, for the owner's approval
//! of the visual change (CLAUDE.md rule 16). It writes raw RGBA files to `RBS_SHOTS_DIR`
//! with Skia's real pixels; it is not a check, so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test picker_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{Overlays, PickerProbe, PickerRow, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, VecModel};

/// A stand-in picture of a screen or window: a background in `bg` with a block of
/// `fg` (test data, so the rows show how a picture sits in its well).
fn picture(bg: [u8; 3], fg: [u8; 3]) -> slint::Image {
    let (w, h) = (192u32, 108u32);
    let mut px = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
    for (i, p) in px.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (i as u32 % w, i as u32 / w);
        let inside = x > w / 6 && x < w * 3 / 4 && y > h / 6 && y < h * 3 / 4;
        let c = if inside { fg } else { bg };
        *p = slint::Rgba8Pixel {
            r: c[0],
            g: c[1],
            b: c[2],
            a: 255,
        };
    }
    slint::Image::from_rgba8(px)
}

#[derive(Default)]
struct R {
    name: &'static str,
    note: &'static str,
    icon: &'static str,
    picture: Option<slint::Image>,
    badge: &'static str,
    badge_icon: &'static str,
    accent: bool,
    group: &'static str,
    font: &'static str,
    action: &'static str,
}

fn rows(list: Vec<R>) -> ModelRc<PickerRow> {
    let mut bands = 0;
    let rows: Vec<PickerRow> = list
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let row = PickerRow {
                name: r.name.into(),
                note: r.note.into(),
                index: i as i32,
                icon: r.icon.into(),
                has_picture: r.picture.is_some(),
                picture: r.picture.unwrap_or_default(),
                badge: r.badge.into(),
                badge_icon: r.badge_icon.into(),
                badge_accent: r.accent,
                group: r.group.into(),
                groups_before: bands,
                font: r.font.into(),
                action: r.action.into(),
            };
            if !r.group.is_empty() {
                bands += 1;
            }
            row
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

struct Screen {
    name: &'static str,
    title: &'static str,
    icon: &'static str,
    hint: &'static str,
    fonts: bool,
    looking: bool,
    status: &'static str,
    empty: (&'static str, &'static str, bool),
    option: (&'static str, bool),
    current: i32,
    rows: fn() -> Vec<R>,
}

fn displays() -> Vec<R> {
    vec![
        R {
            name: "Display 1",
            note: "2560 × 1440",
            icon: "monitor",
            picture: Some(picture([43, 88, 118], [232, 234, 239])),
            badge: "Main",
            action: "Add Display 1",
            ..R::default()
        },
        R {
            name: "Display 2",
            note: "1920 × 1080",
            icon: "monitor",
            picture: Some(picture([19, 78, 94], [250, 250, 250])),
            action: "Add Display 2",
            ..R::default()
        },
        R {
            name: "Display 3",
            note: "3840 × 2160 · Used recently",
            icon: "monitor",
            action: "Add Display 3",
            ..R::default()
        },
    ]
}

fn windows() -> Vec<R> {
    let w = |name, app, group, pic: Option<slint::Image>| R {
        name,
        note: app,
        icon: "window",
        picture: pic,
        group,
        action: "Add window",
        ..R::default()
    };
    vec![
        w(
            "RBS docs",
            "Google Chrome",
            "Google Chrome",
            Some(picture([244, 245, 247], [66, 133, 244])),
        ),
        w(
            "YouTube Studio",
            "Google Chrome",
            "",
            Some(picture([15, 15, 15], [221, 36, 118])),
        ),
        w(
            "components.slint — rbs",
            "Code",
            "Code",
            Some(picture([30, 30, 30], [86, 156, 214])),
        ),
        w("zsh — 120×40", "Terminal", "Terminal", None),
    ]
}

fn cameras() -> Vec<R> {
    vec![
        R {
            name: "Logitech BRIO",
            note: "Has a microphone",
            icon: "camera",
            badge: "Mic",
            badge_icon: "mic",
            action: "Add Logitech BRIO",
            ..R::default()
        },
        R {
            name: "FaceTime HD Camera",
            icon: "camera",
            badge: "Default",
            action: "Add FaceTime HD Camera",
            ..R::default()
        },
        R {
            name: "OBS Virtual Camera",
            icon: "camera",
            action: "Add OBS Virtual Camera",
            ..R::default()
        },
    ]
}

fn existing() -> Vec<R> {
    let s = |name, note, icon, group| R {
        name,
        note,
        icon,
        group,
        action: "Add source",
        ..R::default()
    };
    vec![
        s(
            "Mon_BottomRight",
            "Screen capture",
            "monitor",
            "Screen capture",
        ),
        s("Mon_TopLeft", "Screen capture", "monitor", ""),
        s("Shure", "Audio input", "mic", "Audio input"),
        s("BG", "Image", "image", "Image"),
        s("Logo", "Image", "image", ""),
    ]
}

fn fonts() -> Vec<R> {
    let f = |name: &'static str, group| R {
        name,
        group,
        font: name,
        action: "Use this font",
        ..R::default()
    };
    let mut v = vec![R {
        badge: "Current",
        accent: true,
        ..f("Caladea", "Current")
    }];
    v.push(R {
        font: "",
        ..f("System font", "All fonts")
    });
    for name in [
        "Carlito",
        "DejaVu Sans",
        "DejaVu Serif",
        "FreeSerif",
        "Inter",
        "JetBrains Mono",
        "Liberation Mono",
        "Liberation Serif",
    ] {
        v.push(f(name, ""));
    }
    v
}

const SCREENS: &[Screen] = &[
    Screen {
        name: "display",
        title: "Add screen capture",
        icon: "monitor",
        hint: "Choose the screen to capture.",
        fonts: false,
        looking: false,
        status: "",
        empty: ("", "", false),
        option: ("Capture audio too", false),
        current: 0,
        rows: displays,
    },
    Screen {
        name: "window",
        title: "Add window capture",
        icon: "window",
        hint: "Choose the window to capture.",
        fonts: false,
        looking: false,
        status: "",
        empty: ("", "", false),
        option: ("Capture audio too", true),
        current: 0,
        rows: windows,
    },
    Screen {
        name: "camera",
        title: "Add camera",
        icon: "camera",
        hint: "Choose the camera to add.",
        fonts: false,
        looking: false,
        status: "",
        empty: ("", "", false),
        option: ("Add its microphone too", true),
        current: 0,
        rows: cameras,
    },
    Screen {
        name: "existing",
        title: "Add existing source",
        icon: "plus",
        hint: "Place a source that other scenes already use.",
        fonts: false,
        looking: false,
        status: "",
        empty: ("", "", false),
        option: ("", false),
        current: 3,
        rows: existing,
    },
    Screen {
        name: "font",
        title: "Choose a font",
        icon: "text",
        hint: "The text is drawn in the font you pick.",
        fonts: true,
        looking: false,
        status: "",
        empty: ("", "", false),
        option: ("", false),
        current: 3,
        rows: fonts,
    },
    Screen {
        name: "looking",
        title: "Add screen capture",
        icon: "monitor",
        hint: "Choose the screen to capture.",
        fonts: false,
        looking: true,
        status: "Looking for devices...",
        empty: ("", "", false),
        option: ("", false),
        current: 0,
        rows: Vec::new,
    },
    Screen {
        name: "empty",
        title: "Add camera",
        icon: "camera",
        hint: "Choose the camera to add.",
        fonts: false,
        looking: false,
        status: "",
        empty: (
            "Nothing of this type was found",
            "Connect the device or open what you want to capture, then look again.",
            true,
        ),
        option: ("", false),
        current: 0,
        rows: Vec::new,
    },
];

fn shot(file: &TokenFile, base: Base, b: &str, s: &Screen, dir: &std::path::Path) {
    let theme = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let sl = slot(&theme, "").expect("a slot");
    let w = PickerProbe::new().expect("the window");
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [sl.clone(), sl],
    )
    .expect("the tokens apply");
    let o = w.global::<Overlays>();
    o.set_picker_title(s.title.into());
    o.set_picker_icon(s.icon.into());
    o.set_picker_hint(s.hint.into());
    o.set_picker_fonts(s.fonts);
    o.set_picker_looking(s.looking);
    o.set_picker_status(s.status.into());
    o.set_picker_empty_title(s.empty.0.into());
    o.set_picker_empty_text(s.empty.1.into());
    o.set_picker_can_retry(s.empty.2);
    o.set_picker_option_label(s.option.0.into());
    o.set_picker_option(s.option.1);
    o.set_picker_rows(rows((s.rows)()));
    w.window()
        .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: 2.0 });
    w.show().expect("show");
    o.set_picker_open(true);
    // The picker highlights its first row when it is first drawn; then the screen's row.
    w.window().take_snapshot().expect("a first frame");
    o.set_picker_current(s.current);
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!(
        "picker-{}-{b}-{}x{}.rgba",
        s.name,
        px.width(),
        px.height()
    ));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn picker_screenshots() {
    let dir = std::path::PathBuf::from(std::env::var_os("RBS_SHOTS_DIR").expect("RBS_SHOTS_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let backend = i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            renderer_name: Some("skia".into()),
            ..Default::default()
        },
    );
    slint::platform::set_platform(Box::new(backend)).expect("the testing platform");
    let file = TokenFile::built_in().unwrap();
    for (base, b) in [(Base::Dark, "dark"), (Base::Light, "light")] {
        for s in SCREENS {
            shot(&file, base, b, s, &dir);
        }
    }
}
