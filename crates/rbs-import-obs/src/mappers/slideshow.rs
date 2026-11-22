//! The OBS Image Slide Show (`slideshow`, `slideshow_v2`) as an RBS slide show of one
//! folder (ADR-0171 decision 11).

use std::path::{Path, PathBuf};

use rbs_scene::activity::{OnReturn, Run};
use rbs_scene::model::SettingValue;
use serde_json::Value;

use super::sources::rbs::{self, keys};
use super::{MapContext, MappedSource, SourceMapper, SourceOutcome};
use crate::obs::{ObsSource, flag, text, uint};

/// The slide show's own settings keys (`rbs_sources::slideshow`); `rbs-cli` tests that
/// they match the real schema.
pub mod slide_keys {
    /// The folder.
    pub const FOLDER: &str = "folder";
    /// `auto` or `manual`.
    pub const ADVANCE: &str = "advance";
    /// Seconds each picture shows.
    pub const INTERVAL_S: &str = "interval_s";
    /// `sequential` or `shuffle`.
    pub const ORDER: &str = "order";
    /// Start again after the last picture.
    pub const LOOP: &str = "loop";
    /// `cut`, `fade`, `slide` or `swipe`.
    pub const TRANSITION: &str = "transition";
    /// Milliseconds of a change.
    pub const TRANSITION_MS: &str = "transition_ms";
    /// `contain`, `cover` or `stretch`.
    pub const FIT: &str = "fit";
    /// Every key this mapper writes besides `width` and `height`.
    pub const ALL: [&str; 8] = [
        FOLDER,
        ADVANCE,
        INTERVAL_S,
        ORDER,
        LOOP,
        TRANSITION,
        TRANSITION_MS,
        FIT,
    ];
}

use slide_keys as k;

/// Picture names RBS shows (ADR-0171 decision 2).
fn is_picture(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        ["png", "jpg", "jpeg"]
            .iter()
            .any(|p| e.eq_ignore_ascii_case(p))
    })
}

/// A name OBS shows that RBS does not (bmp, tga, gif, webp, ...).
fn is_other_picture(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        ["bmp", "tga", "gif", "webp", "psd", "jxr"]
            .iter()
            .any(|p| e.eq_ignore_ascii_case(p))
    })
}

/// The OBS slide show mapper; `self.0` is the OBS id.
pub struct Slideshow(pub &'static str);

impl SourceMapper for Slideshow {
    fn obs_id(&self) -> &'static str {
        self.0
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::SLIDESHOW]
    }
    fn map(&self, src: &ObsSource, ctx: &mut MapContext<'_>) -> SourceOutcome {
        let s = &src.settings;
        let entries: Vec<PathBuf> = s
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|f| f.get("value").and_then(Value::as_str))
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
            .collect();
        let Some(first) = entries.first() else {
            return SourceOutcome::skip("The slide show has no pictures.");
        };
        // A listed file stands for its folder; an entry that is not a file name is a
        // folder OBS reads whole.
        let folder_of = |p: &Path| -> PathBuf {
            if is_picture(p) || is_other_picture(p) {
                p.parent().map(Path::to_path_buf).unwrap_or_default()
            } else {
                p.to_path_buf()
            }
        };
        let folder = folder_of(first);
        let mut m = MappedSource::new(rbs::SLIDESHOW);
        if entries.iter().any(|e| folder_of(e) != folder) {
            m.notes.push(
                "The pictures are in more than one folder; only the first folder is shown."
                    .to_owned(),
            );
        }
        if entries.iter().any(|e| is_picture(e) || is_other_picture(e)) {
            m.notes.push(
                "RBS shows every PNG and JPEG picture in the folder, not only the files OBS listed."
                    .to_owned(),
            );
        }
        if entries.iter().any(|e| is_other_picture(e)) {
            m.notes.push(
                "Only PNG and JPEG pictures are shown; other formats are skipped.".to_owned(),
            );
        }
        let txt = |v: &str| SettingValue::Text(v.to_owned());
        m.settings
            .insert(k::FOLDER.to_owned(), txt(&folder.display().to_string()));
        let manual = text(s, "slide_mode") == "mode_manual";
        m.settings.insert(
            k::ADVANCE.to_owned(),
            txt(if manual { "manual" } else { "auto" }),
        );
        // OBS keeps milliseconds; RBS shows each picture 1 s to 1 h in half seconds.
        let ms = uint(s, "slide_time", 8_000);
        let interval = ((ms as f64 / 500.0).round() / 2.0).clamp(1.0, 3600.0);
        if (interval * 1000.0 - ms as f64).abs() >= 1.0 {
            m.notes
                .push(format!("Each picture shows {interval} s (RBS's nearest)."));
        }
        m.settings
            .insert(k::INTERVAL_S.to_owned(), SettingValue::Float(interval));
        let shuffle = flag(s, "randomize", false);
        m.settings.insert(
            k::ORDER.to_owned(),
            txt(if shuffle { "shuffle" } else { "sequential" }),
        );
        m.settings.insert(
            k::LOOP.to_owned(),
            SettingValue::Bool(flag(s, "loop", true)),
        );
        let transition = match text(s, "transition") {
            "cut" => "cut",
            "slide" => "slide",
            "swipe" => "swipe",
            _ => "fade",
        };
        m.settings.insert(k::TRANSITION.to_owned(), txt(transition));
        let speed = uint(s, "transition_speed", 700).min(5_000);
        m.settings.insert(
            k::TRANSITION_MS.to_owned(),
            SettingValue::Float((speed / 50 * 50) as f64),
        );
        // OBS scales each picture into the source's box, keeping its shape.
        m.settings.insert(k::FIT.to_owned(), txt("contain"));
        let size = text(s, "use_custom_size")
            .split_once('x')
            .and_then(|(w, h)| Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?)))
            .filter(|&(w, h)| (1..=8192).contains(&w) && (1..=8192).contains(&h));
        let custom = text(s, "use_custom_size");
        if size.is_none() && !custom.is_empty() && custom != "Automatic" {
            m.notes.push(
                "The size is the canvas's; RBS has no aspect-ratio size for a slide show."
                    .to_owned(),
            );
        }
        let (w, h) = size.unwrap_or(ctx.canvas);
        m.settings
            .insert(keys::WIDTH.to_owned(), SettingValue::Int(i64::from(w)));
        m.settings
            .insert(keys::HEIGHT.to_owned(), SettingValue::Int(i64::from(h)));
        m.native = Some((w, h));
        if flag(s, "hide", false) {
            m.notes.push(
                "RBS keeps the last picture instead of hiding the slide show when it ends."
                    .to_owned(),
            );
        }
        match text(s, "playback_behavior") {
            "stop_restart" => m.notes.push(
                "RBS continues where it was when the slide show is shown again; it does not restart."
                    .to_owned(),
            ),
            "pause_unpause" => {}
            // OBS's default plays while hidden, as an RBS source active while placed.
            _ => m.activity.run = Some(Run::Placed),
        }
        m.activity.on_return = Some(OnReturn::Resume);
        SourceOutcome::Mapped(m)
    }
}
