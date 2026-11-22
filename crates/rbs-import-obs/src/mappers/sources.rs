//! Source mappers of ADR-0131 decision 7.

use std::path::Path;

use rbs_scene::activity::OnReturn;
use rbs_scene::model::SettingValue;
use serde_json::{Map, Value};

use super::{MapContext, MappedSource, SourceMapper, SourceOutcome, obs_color};
use crate::env::LookupTarget;
use crate::obs::{ObsSource, flag, num, text, uint};

/// RBS source types (`rbs_sources::types`), as strings: `rbs-app` tests that they
/// match the real registry (ADR-0131 decision 1).
pub mod rbs {
    /// Display capture.
    pub const DISPLAY: &str = "org.rbs.source.display";
    /// Window capture.
    pub const WINDOW: &str = "org.rbs.source.window";
    /// Camera.
    pub const CAMERA: &str = "org.rbs.source.camera";
    /// Audio input.
    pub const AUDIO_INPUT: &str = "org.rbs.source.audio-input";
    /// Audio output (loopback).
    pub const AUDIO_OUTPUT: &str = "org.rbs.source.audio-output";
    /// Application audio.
    pub const APP_AUDIO: &str = "org.rbs.source.application-audio";
    /// PNG or JPEG picture.
    pub const IMAGE: &str = "org.rbs.source.image";
    /// Colour or gradient.
    pub const COLOR: &str = "org.rbs.source.color";
    /// Media file.
    pub const MEDIA: &str = "org.rbs.source.media";
    /// Browser page.
    pub const BROWSER: &str = "org.rbs.source.browser";

    /// A slide show of one folder (ADR-0171).
    pub const SLIDESHOW: &str = "org.rbs.source.slideshow";

    /// Every type a source mapper may produce.
    pub const ALL: [&str; 11] = [
        DISPLAY,
        WINDOW,
        CAMERA,
        AUDIO_INPUT,
        AUDIO_OUTPUT,
        APP_AUDIO,
        IMAGE,
        COLOR,
        MEDIA,
        BROWSER,
        SLIDESHOW,
    ];

    /// Settings keys (`rbs_sources::types::keys`).
    pub mod keys {
        /// Adapter id, stored with a target.
        pub const ADAPTER: &str = "adapter";
        /// Target id.
        pub const TARGET: &str = "target";
        /// Draw the cursor.
        pub const CURSOR: &str = "cursor";
        /// Width.
        pub const WIDTH: &str = "width";
        /// Height.
        pub const HEIGHT: &str = "height";
        /// Frame rate.
        pub const FPS: &str = "fps";
        /// File path.
        pub const PATH: &str = "path";
        /// Colour.
        pub const COLOR: &str = "color";
        /// Loop a media file.
        pub const LOOP: &str = "loop";
        /// Browser URL.
        pub const URL: &str = "url";
        /// Browser page keeps its transparency.
        pub const TRANSPARENT: &str = "transparent";
        /// Every key a source mapper may write.
        pub const ALL: [&str; 11] = [
            ADAPTER,
            TARGET,
            CURSOR,
            WIDTH,
            HEIGHT,
            FPS,
            PATH,
            COLOR,
            LOOP,
            URL,
            TRANSPARENT,
        ];
    }
}

use rbs::keys;

/// Hint keys of ADR-0077.
const TARGET_TITLE: &str = "target_title";
const TARGET_APP: &str = "target_app";

const NOT_CHECKED: &str = "This computer's devices were not checked, so RBS uses the default one.";

/// Every built-in source mapper.
pub fn builtin() -> Vec<Box<dyn SourceMapper>> {
    let mut v: Vec<Box<dyn SourceMapper>> = Vec::new();
    for id in [
        "monitor_capture",
        "display_capture",
        "xshm_input",
        "pipewire-desktop-capture-source",
    ] {
        v.push(Box::new(Display(id)));
    }
    v.push(Box::new(ScreenCapture));
    for id in [
        "window_capture",
        "xcomposite_input",
        "pipewire-window-capture-source",
    ] {
        v.push(Box::new(Window(id)));
    }
    v.push(Box::new(GameCapture));
    for id in [
        "dshow_input",
        "av_capture_input",
        "av_capture_input_v2",
        "macos-avcapture",
        "v4l2_input",
    ] {
        v.push(Box::new(Camera(id)));
    }
    for id in [
        "wasapi_input_capture",
        "coreaudio_input_capture",
        "pulse_input_capture",
        "alsa_input_capture",
    ] {
        v.push(Box::new(Audio(id, rbs::AUDIO_INPUT)));
    }
    for id in [
        "wasapi_output_capture",
        "coreaudio_output_capture",
        "pulse_output_capture",
    ] {
        v.push(Box::new(Audio(id, rbs::AUDIO_OUTPUT)));
    }
    v.push(Box::new(ProcessAudio));
    v.push(Box::new(MacAudio));
    v.push(Box::new(Image));
    for id in ["color_source", "color_source_v2", "color_source_v3"] {
        v.push(Box::new(Color(id)));
    }
    v.push(Box::new(Media));
    v.push(Box::new(Browser));
    for id in ["slideshow", "slideshow_v2"] {
        v.push(Box::new(super::slideshow::Slideshow(id)));
    }
    v
}

fn set_target(m: &mut MappedSource, t: &LookupTarget) {
    m.settings.insert(
        keys::ADAPTER.to_owned(),
        SettingValue::Text(t.adapter.clone()),
    );
    m.settings.insert(
        keys::TARGET.to_owned(),
        SettingValue::Text(t.target.clone()),
    );
    if m.native.is_none() {
        m.native = t.size;
    }
}

/// The single target whose id or name matches, compared without case for names.
fn find_target<'t>(
    targets: &'t [LookupTarget],
    id: &str,
    name: &str,
    app: &str,
) -> Option<&'t LookupTarget> {
    if !id.is_empty()
        && let Some(t) = targets.iter().find(|t| t.target == id)
    {
        return Some(t);
    }
    if name.is_empty() {
        return None;
    }
    let mut found = targets.iter().filter(|t| {
        t.name.eq_ignore_ascii_case(name)
            && (app.is_empty() || t.app.as_deref().is_none_or(|a| a.eq_ignore_ascii_case(app)))
    });
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

fn display(ctx: &mut MapContext<'_>, cursor: bool) -> SourceOutcome {
    let mut m = MappedSource::new(rbs::DISPLAY);
    m.settings
        .insert(keys::CURSOR.to_owned(), SettingValue::Bool(cursor));
    let targets = ctx.targets(rbs::DISPLAY);
    match targets {
        [] => m.notes.push(NOT_CHECKED.to_owned()),
        [only] => {
            let only = only.clone();
            set_target(&mut m, &only);
        }
        _ => m.notes.push(
            "OBS does not say which display it was in a way RBS can match, so RBS uses the main display."
                .to_owned(),
        ),
    }
    SourceOutcome::Mapped(m)
}

struct Display(&'static str);

impl SourceMapper for Display {
    fn obs_id(&self) -> &'static str {
        self.0
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::DISPLAY]
    }
    fn map(&self, src: &ObsSource, ctx: &mut MapContext<'_>) -> SourceOutcome {
        let s = &src.settings;
        let cursor = match self.0 {
            "monitor_capture" => flag(s, "capture_cursor", true),
            "pipewire-desktop-capture-source" => flag(s, "ShowCursor", true),
            _ => flag(s, "show_cursor", true),
        };
        display(ctx, cursor)
    }
}

/// Parsed OBS window text: title, class or app.
#[derive(Debug, Default, PartialEq, Eq)]
struct WindowText {
    title: String,
    app: String,
}

/// Windows `title:class:exe`, where `#3A` stands for `:` inside a part.
fn parse_windows_window(text: &str) -> WindowText {
    let mut parts = text.split(':').map(|p| p.replace("#3A", ":"));
    let title = parts.next().unwrap_or_default();
    let _class = parts.next();
    let app = parts.next().unwrap_or_default();
    WindowText { title, app }
}

/// xcomposite `id\r\nname\r\nclass`.
fn parse_x11_window(text: &str) -> WindowText {
    let mut parts = text.split("\r\n");
    let _id = parts.next();
    WindowText {
        title: parts.next().unwrap_or_default().to_owned(),
        app: parts.next().unwrap_or_default().to_owned(),
    }
}

fn window(
    ctx: &mut MapContext<'_>,
    w: &WindowText,
    cursor: bool,
    approximate: Option<&str>,
) -> SourceOutcome {
    let mut m = MappedSource::new(rbs::WINDOW);
    m.settings
        .insert(keys::CURSOR.to_owned(), SettingValue::Bool(cursor));
    if !w.title.is_empty() {
        m.hints.insert(TARGET_TITLE.to_owned(), w.title.clone());
    }
    if !w.app.is_empty() {
        m.hints.insert(TARGET_APP.to_owned(), w.app.clone());
    }
    if let Some(note) = approximate {
        m.notes.push(note.to_owned());
    }
    let targets = ctx.targets(rbs::WINDOW);
    if targets.is_empty() {
        m.notes.push(
            "This computer's windows were not checked; pick the window in the source's settings."
                .to_owned(),
        );
    } else if let Some(t) = find_target(targets, "", &w.title, &w.app) {
        let t = t.clone();
        set_target(&mut m, &t);
    } else {
        m.notes.push(
            "The window is not open now; RBS finds it by its title and application when it opens."
                .to_owned(),
        );
    }
    SourceOutcome::Mapped(m)
}

fn windows_or_mac_window(s: &Map<String, Value>) -> WindowText {
    let w = text(s, "window");
    if w.contains(':') {
        parse_windows_window(w)
    } else {
        WindowText {
            title: text(s, "window_name").to_owned(),
            app: text(s, "owner_name").to_owned(),
        }
    }
}

struct Window(&'static str);

impl SourceMapper for Window {
    fn obs_id(&self) -> &'static str {
        self.0
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::WINDOW]
    }
    fn map(&self, src: &ObsSource, ctx: &mut MapContext<'_>) -> SourceOutcome {
        let s = &src.settings;
        match self.0 {
            "xcomposite_input" => window(
                ctx,
                &parse_x11_window(text(s, "capture_window")),
                flag(s, "show_cursor", true),
                None,
            ),
            "pipewire-window-capture-source" => window(
                ctx,
                &WindowText::default(),
                flag(s, "ShowCursor", true),
                Some(
                    "OBS keeps only a PipeWire permission for this window; pick the window again.",
                ),
            ),
            _ => window(
                ctx,
                &windows_or_mac_window(s),
                flag(s, "cursor", true) && flag(s, "show_cursor", true),
                None,
            ),
        }
    }
}

/// macOS `screen_capture`: display (type 0), window (1) or application (2).
struct ScreenCapture;

impl SourceMapper for ScreenCapture {
    fn obs_id(&self) -> &'static str {
        "screen_capture"
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::DISPLAY, rbs::WINDOW]
    }
    fn map(&self, src: &ObsSource, ctx: &mut MapContext<'_>) -> SourceOutcome {
        let s = &src.settings;
        let cursor = flag(s, "show_cursor", true);
        match uint(s, "type", 0) {
            0 => display(ctx, cursor),
            1 => window(
                ctx,
                &WindowText {
                    title: text(s, "window_name").to_owned(),
                    app: text(s, "owner_name").to_owned(),
                },
                cursor,
                None,
            ),
            _ => window(
                ctx,
                &WindowText {
                    title: String::new(),
                    app: text(s, "application").to_owned(),
                },
                cursor,
                Some("OBS captured a whole application; RBS captures one of its windows."),
            ),
        }
    }
}

struct GameCapture;

impl SourceMapper for GameCapture {
    fn obs_id(&self) -> &'static str {
        "game_capture"
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::WINDOW]
    }
    fn map(&self, src: &ObsSource, ctx: &mut MapContext<'_>) -> SourceOutcome {
        let s = &src.settings;
        let mode = text(s, "capture_mode");
        if mode == "any_fullscreen" || mode == "hotkey" || text(s, "window").is_empty() {
            return SourceOutcome::Skipped {
                reason: "Game capture of any full-screen application is not in RBS.".to_owned(),
                hint: Some("Add a window source for the game.".to_owned()),
            };
        }
        window(
            ctx,
            &parse_windows_window(text(s, "window")),
            flag(s, "capture_cursor", true),
            Some("RBS captures the game's window; OBS hooked into the game."),
        )
    }
}

struct Camera(&'static str);

impl SourceMapper for Camera {
    fn obs_id(&self) -> &'static str {
        self.0
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::CAMERA]
    }
    fn map(&self, src: &ObsSource, ctx: &mut MapContext<'_>) -> SourceOutcome {
        let s = &src.settings;
        let mut m = MappedSource::new(rbs::CAMERA);
        let (id, name) = match self.0 {
            "dshow_input" => {
                // `Name:\\?\usb#...`: the name, then the device path.
                let raw = text(s, "video_device_id");
                let name = raw.split(":\\\\").next().unwrap_or("").replace("#3A", ":");
                (raw.to_owned(), name)
            }
            "v4l2_input" => (text(s, "device_id").to_owned(), String::new()),
            _ => (
                text(s, "device").to_owned(),
                text(s, "device_name").to_owned(),
            ),
        };
        if !name.is_empty() {
            m.hints.insert(TARGET_TITLE.to_owned(), name.clone());
        }
        if self.0 == "dshow_input" && uint(s, "res_type", 0) == 1 {
            let res = text(s, "resolution");
            if let Some((w, h)) = res.split_once('x')
                && let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>())
                && w > 0
                && h > 0
            {
                m.settings
                    .insert(keys::WIDTH.to_owned(), SettingValue::Int(i64::from(w)));
                m.settings
                    .insert(keys::HEIGHT.to_owned(), SettingValue::Int(i64::from(h)));
                m.native = Some((w, h));
            }
            // frame_interval is in 100 ns units; -1 or 0 is "the device's default".
            let fi = s
                .get("frame_interval")
                .and_then(Value::as_i64)
                .unwrap_or(-1);
            if fi > 0 {
                let fps = (10_000_000.0 / fi as f64).round() as i64;
                if fps > 0 {
                    m.settings
                        .insert(keys::FPS.to_owned(), SettingValue::Int(fps));
                }
            }
        }
        let targets = ctx.targets(rbs::CAMERA);
        if targets.is_empty() {
            m.notes.push(NOT_CHECKED.to_owned());
        } else if let Some(t) = find_target(targets, &id, &name, "") {
            let t = t.clone();
            set_target(&mut m, &t);
        } else {
            m.notes.push(
                "The camera is not connected now; RBS finds it by its name when it is.".to_owned(),
            );
        }
        SourceOutcome::Mapped(m)
    }
}

fn audio_device(ctx: &mut MapContext<'_>, type_id: &'static str, id: &str) -> MappedSource {
    let mut m = MappedSource::new(type_id);
    m.has_audio = true;
    if id.is_empty() || id == "default" {
        return m;
    }
    let targets = ctx.targets(type_id);
    if targets.is_empty() {
        m.notes.push(NOT_CHECKED.to_owned());
    } else if let Some(t) = find_target(targets, id, "", "") {
        let t = t.clone();
        set_target(&mut m, &t);
    } else {
        m.notes.push(
            "The audio device was not found on this computer, so RBS uses the default one."
                .to_owned(),
        );
    }
    m
}

struct Audio(&'static str, &'static str);

impl SourceMapper for Audio {
    fn obs_id(&self) -> &'static str {
        self.0
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        if self.1 == rbs::AUDIO_INPUT {
            &[rbs::AUDIO_INPUT]
        } else {
            &[rbs::AUDIO_OUTPUT]
        }
    }
    fn map(&self, src: &ObsSource, ctx: &mut MapContext<'_>) -> SourceOutcome {
        SourceOutcome::Mapped(audio_device(ctx, self.1, text(&src.settings, "device_id")))
    }
}

/// Windows `wasapi_process_output_capture`: one application's sound.
struct ProcessAudio;

impl SourceMapper for ProcessAudio {
    fn obs_id(&self) -> &'static str {
        "wasapi_process_output_capture"
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::APP_AUDIO]
    }
    fn map(&self, src: &ObsSource, _ctx: &mut MapContext<'_>) -> SourceOutcome {
        let w = parse_windows_window(text(&src.settings, "window"));
        let mut m = MappedSource::new(rbs::APP_AUDIO);
        m.has_audio = true;
        if !w.app.is_empty() {
            m.hints.insert(TARGET_APP.to_owned(), w.app);
        }
        m.notes
            .push("Pick the application again in the source's settings.".to_owned());
        SourceOutcome::Mapped(m)
    }
}

/// macOS `sck_audio_capture`: desktop audio (type 0) or one application (type 1).
struct MacAudio;

impl SourceMapper for MacAudio {
    fn obs_id(&self) -> &'static str {
        "sck_audio_capture"
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::AUDIO_OUTPUT, rbs::APP_AUDIO]
    }
    fn map(&self, src: &ObsSource, ctx: &mut MapContext<'_>) -> SourceOutcome {
        let s = &src.settings;
        if uint(s, "type", 0) == 0 {
            return SourceOutcome::Mapped(audio_device(ctx, rbs::AUDIO_OUTPUT, ""));
        }
        let mut m = MappedSource::new(rbs::APP_AUDIO);
        m.has_audio = true;
        let app = text(s, "application");
        if !app.is_empty() {
            m.hints.insert(TARGET_APP.to_owned(), app.to_owned());
        }
        m.notes
            .push("Pick the application again in the source's settings.".to_owned());
        SourceOutcome::Mapped(m)
    }
}

struct Image;

impl SourceMapper for Image {
    fn obs_id(&self) -> &'static str {
        "image_source"
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::IMAGE]
    }
    fn map(&self, src: &ObsSource, ctx: &mut MapContext<'_>) -> SourceOutcome {
        let file = text(&src.settings, "file");
        if file.is_empty() {
            return SourceOutcome::skip("The image source has no file.");
        }
        let shown = Path::new(file)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                ["png", "jpg", "jpeg"]
                    .iter()
                    .any(|p| e.eq_ignore_ascii_case(p))
            });
        if !shown {
            return SourceOutcome::Skipped {
                reason: "Only PNG and JPEG pictures can be shown for now.".to_owned(),
                hint: Some("Save the picture as PNG and add it as an image source.".to_owned()),
            };
        }
        let mut m = MappedSource::new(rbs::IMAGE);
        m.settings
            .insert(keys::PATH.to_owned(), SettingValue::Text(file.to_owned()));
        let path = Path::new(file);
        m.native = ctx.lookup().picture_size(path);
        if m.native.is_none() {
            m.notes.push(if ctx.lookup().file_exists(path) {
                "The picture could not be read.".to_owned()
            } else {
                "The picture file was not found on this computer; pick it again in the source's settings."
                    .to_owned()
            });
        }
        SourceOutcome::Mapped(m)
    }
}

struct Color(&'static str);

impl SourceMapper for Color {
    fn obs_id(&self) -> &'static str {
        self.0
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::COLOR]
    }
    fn map(&self, src: &ObsSource, ctx: &mut MapContext<'_>) -> SourceOutcome {
        let s = &src.settings;
        let default = if self.0 == "color_source" {
            0xFFFF_FFFF
        } else {
            0xFFD1_D1D1
        };
        let mut m = MappedSource::new(rbs::COLOR);
        m.settings.insert(
            keys::COLOR.to_owned(),
            SettingValue::Text(obs_color(uint(s, "color", default))),
        );
        let w = (uint(s, "width", u64::from(ctx.canvas.0)) as u32).clamp(1, 8192);
        let h = (uint(s, "height", u64::from(ctx.canvas.1)) as u32).clamp(1, 8192);
        m.settings
            .insert(keys::WIDTH.to_owned(), SettingValue::Int(i64::from(w)));
        m.settings
            .insert(keys::HEIGHT.to_owned(), SettingValue::Int(i64::from(h)));
        m.native = Some((w, h));
        SourceOutcome::Mapped(m)
    }
}

struct Media;

impl SourceMapper for Media {
    fn obs_id(&self) -> &'static str {
        "ffmpeg_source"
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::MEDIA]
    }
    fn map(&self, src: &ObsSource, ctx: &mut MapContext<'_>) -> SourceOutcome {
        let s = &src.settings;
        if !flag(s, "is_local_file", true) {
            return SourceOutcome::skip("Network streams in a media source are not in RBS yet.");
        }
        let file = text(s, "local_file");
        if file.is_empty() {
            return SourceOutcome::skip("The media source has no file.");
        }
        let mut m = MappedSource::new(rbs::MEDIA);
        m.has_audio = true;
        m.settings
            .insert(keys::PATH.to_owned(), SettingValue::Text(file.to_owned()));
        m.settings.insert(
            keys::LOOP.to_owned(),
            SettingValue::Bool(flag(s, "looping", false)),
        );
        // "Restart playback when source becomes active" (ADR-0152 decision 10).
        m.activity.on_return = Some(if flag(s, "restart_on_activate", true) {
            OnReturn::Restart
        } else {
            OnReturn::Resume
        });
        if !ctx.lookup().file_exists(Path::new(file)) {
            m.notes.push(
                "The media file was not found on this computer; pick it again in the source's settings."
                    .to_owned(),
            );
        }
        if num(s, "speed_percent", 100.0) != 100.0 {
            m.notes
                .push("Playback speed is not in RBS; the file plays at normal speed.".to_owned());
        }
        SourceOutcome::Mapped(m)
    }
}

/// OBS's default browser CSS: anything else is reported.
const OBS_DEFAULT_CSS: &str =
    "body { background-color: rgba(0, 0, 0, 0); margin: 0px auto; overflow: hidden; }";

struct Browser;

impl SourceMapper for Browser {
    fn obs_id(&self) -> &'static str {
        "browser_source"
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::BROWSER]
    }
    fn map(&self, src: &ObsSource, _ctx: &mut MapContext<'_>) -> SourceOutcome {
        let s = &src.settings;
        let url = if flag(s, "is_local_file", false) {
            let file = text(s, "local_file");
            if file.is_empty() {
                return SourceOutcome::skip("The browser source has no file.");
            }
            let path = file.replace('\\', "/");
            if path.starts_with('/') {
                format!("file://{path}")
            } else {
                format!("file:///{path}")
            }
        } else {
            text(s, "url").to_owned()
        };
        if url.is_empty() {
            return SourceOutcome::skip("The browser source has no address.");
        }
        let mut m = MappedSource::new(rbs::BROWSER);
        m.settings
            .insert(keys::URL.to_owned(), SettingValue::Text(url));
        let even = |v: u64| -> (u32, bool) {
            let c = (v.clamp(2, 8192) as u32) & !1;
            (c, u64::from(c) != v)
        };
        let (w, wc) = even(uint(s, "width", 800));
        let (h, hc) = even(uint(s, "height", 600));
        if wc || hc {
            m.notes
                .push("The page size was rounded to even pixels.".to_owned());
        }
        m.settings
            .insert(keys::WIDTH.to_owned(), SettingValue::Int(i64::from(w)));
        m.settings
            .insert(keys::HEIGHT.to_owned(), SettingValue::Int(i64::from(h)));
        m.native = Some((w, h));
        if flag(s, "fps_custom", false) {
            let fps = uint(s, "fps", 30).clamp(1, 60);
            m.settings
                .insert(keys::FPS.to_owned(), SettingValue::Int(fps as i64));
        }
        m.settings
            .insert(keys::TRANSPARENT.to_owned(), SettingValue::Bool(true));
        let css = text(s, "css");
        if s.contains_key("css") && css.trim() != OBS_DEFAULT_CSS {
            m.notes.push("Custom CSS is not in RBS.".to_owned());
        }
        // "Refresh browser when scene becomes active" (ADR-0152 decision 10); off
        // is the type's default, `continue`.
        if flag(s, "restart_when_active", false) {
            m.activity.on_return = Some(OnReturn::Restart);
        }
        if flag(s, "reroute_audio", false) {
            m.notes
                .push("RBS does not take sound from browser pages yet.".to_owned());
        }
        SourceOutcome::Mapped(m)
    }
}

/// Why a source of an OBS type without a mapper was not imported.
pub fn unmapped_reason(obs_id: &str) -> (String, Option<String>) {
    let label = crate::obs::type_label(obs_id);
    match label {
        "Text" => (
            "Text sources are not in RBS yet.".to_owned(),
            Some("A later RBS can add it from the saved report.".to_owned()),
        ),
        "Source" => (
            "This source type comes from an OBS plugin or version RBS does not know.".to_owned(),
            None,
        ),
        _ => (format!("{label} sources are not in RBS yet."), None),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn windows_window_text_unescapes_colons() {
        assert_eq!(
            parse_windows_window("Chat#3A Live:Chrome_WidgetWin_1:chrome.exe"),
            WindowText {
                title: "Chat: Live".into(),
                app: "chrome.exe".into()
            }
        );
    }

    #[test]
    fn x11_window_text_has_three_lines() {
        assert_eq!(
            parse_x11_window("0x3a00007\r\nTerminal\r\ngnome-terminal"),
            WindowText {
                title: "Terminal".into(),
                app: "gnome-terminal".into()
            }
        );
    }

    #[test]
    fn a_name_matches_only_one_target() {
        let t = vec![
            LookupTarget::new("a", "1", "Cam"),
            LookupTarget::new("a", "2", "Cam"),
            LookupTarget::new("a", "3", "Other"),
        ];
        assert!(find_target(&t, "", "Cam", "").is_none());
        assert_eq!(find_target(&t, "", "other", "").unwrap().target, "3");
        assert_eq!(find_target(&t, "2", "x", "").unwrap().target, "2");
    }
}
