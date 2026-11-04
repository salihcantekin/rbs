//! The choices the Recording settings offer for the video encoder's quality settings,
//! built from the saved profile and what the picked encoder can do (its capabilities),
//! so the form never offers a setting the encoder refuses. Toolkit-free and tested
//! without a window; the window shows the labels and maps a picked index back.

use crate::record_profile::{
    KEYFRAME_SECONDS, MAX_B_FRAMES, Preset, QUALITY_LEVELS, RateMode, RecordProfile, VIDEO_KBPS,
};
use crate::studio_host::EncoderOptions;

/// Options of one field and the one in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choices<T> {
    /// Label and value of each option, in the order shown.
    pub items: Vec<(String, T)>,
    /// The option in use.
    pub index: usize,
}

impl<T: Clone + PartialEq> Choices<T> {
    /// `items`, with `current` added (labelled by `label`) when it is not one of them,
    /// so the field always shows what the profile holds.
    fn with_current(mut items: Vec<(String, T)>, current: T, label: impl Fn(&T) -> String) -> Self {
        let index = match items.iter().position(|(_, v)| *v == current) {
            Some(i) => i,
            None => {
                items.push((label(&current), current));
                items.len() - 1
            }
        };
        Self { items, index }
    }

    /// The labels, for the window.
    pub fn labels(&self) -> impl Iterator<Item = String> + '_ {
        self.items.iter().map(|(l, _)| l.clone())
    }

    /// The value at `i`, if there is one.
    pub fn get(&self, i: i32) -> Option<T> {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.items.get(i))
            .map(|(_, v)| v.clone())
    }
}

/// Everything the encoder part of the Recording settings shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderForm {
    /// CBR, VBR, constant quality: the modes the encoder has.
    pub rate_modes: Choices<RateMode>,
    /// The bitrate field's label: the CBR rate or the VBR target.
    pub bitrate_label: &'static str,
    /// VBR peak: automatic (1.5 x the target) or a rate at least the target.
    pub peaks: Choices<Option<u32>>,
    /// Constant-quality levels.
    pub qualities: Choices<u8>,
    /// Keyframe intervals in seconds.
    pub keyframes: Choices<u32>,
    /// Automatic, or one of the encoder's H.264 profiles.
    pub profiles: Choices<Option<String>>,
    /// Quality, balanced, speed.
    pub presets: Choices<Preset>,
    /// 0 up to the encoder's most B-frames.
    pub b_frames: Choices<u32>,
    /// Why B-frames cannot be changed, or `None`.
    pub b_frames_note: Option<&'static str>,
    /// The encoder can pad CBR with filler data.
    pub padding_available: bool,
}

fn profile_label(p: &str) -> String {
    match p {
        "high" => "High".into(),
        "main" => "Main".into(),
        "baseline" => "Baseline".into(),
        other => other.to_owned(),
    }
}

impl EncoderForm {
    /// The form for `profile` with an encoder that can do `options`.
    pub fn new(profile: &RecordProfile, options: &EncoderOptions) -> Self {
        let has = |r: &str| options.rate_controls.iter().any(|c| c == r);
        let modes = RateMode::ALL
            .into_iter()
            .filter(|m| options.rate_controls.is_empty() || has(m.capability()))
            .map(|m| (m.label().to_owned(), m))
            .collect();
        let rate_modes =
            Choices::with_current(modes, profile.rate_control, |m| m.label().to_owned());

        let auto = format!(
            "Automatic ({} kbit/s, 1.5 x the target bitrate)",
            profile.video_kbps.saturating_mul(3) / 2
        );
        let mut peaks = vec![(auto, None)];
        peaks.extend(
            VIDEO_KBPS
                .iter()
                .filter(|k| **k >= profile.video_kbps)
                .map(|k| (format!("{k} kbit/s"), Some(*k))),
        );
        let peaks = Choices::with_current(peaks, profile.max_video_kbps, |k| {
            k.map_or_else(String::new, |k| format!("{k} kbit/s"))
        });

        let qualities = QUALITY_LEVELS
            .iter()
            .map(|q| {
                let label = match q {
                    14 => "14 (best, largest files)".to_owned(),
                    23 => "23 (default)".to_owned(),
                    36 => "36 (smallest files)".to_owned(),
                    q => q.to_string(),
                };
                (label, *q)
            })
            .collect();
        let qualities = Choices::with_current(qualities, profile.quality, u8::to_string);

        let keyframes = KEYFRAME_SECONDS
            .iter()
            .map(|s| {
                let label = if *s == 2 {
                    "2 s (default)".to_owned()
                } else {
                    format!("{s} s")
                };
                (label, *s)
            })
            .collect();
        let keyframes =
            Choices::with_current(keyframes, profile.keyframe_seconds, |s| format!("{s} s"));

        let mut profiles = vec![("Automatic".to_owned(), None)];
        for p in ["high", "main", "baseline"] {
            if options.h264_profiles.iter().any(|h| h == p) {
                profiles.push((profile_label(p), Some(p.to_owned())));
            }
        }
        let profiles = Choices::with_current(profiles, profile.profile.clone(), |p| {
            p.as_deref().map_or_else(String::new, profile_label)
        });

        let presets = Preset::ALL
            .into_iter()
            .map(|p| (p.label().to_owned(), p))
            .collect();
        let presets = Choices::with_current(presets, profile.preset, |p| p.label().to_owned());

        let most = options.b_frames_max.min(MAX_B_FRAMES);
        let b_frames = (0..=most).map(|n| (n.to_string(), n)).collect();
        let b_frames = Choices::with_current(b_frames, profile.b_frames, |n| {
            format!("{n} (this encoder uses {most})")
        });

        Self {
            rate_modes,
            bitrate_label: if profile.rate_control == RateMode::Vbr {
                "Target bitrate"
            } else {
                "Video bitrate"
            },
            peaks,
            qualities,
            keyframes,
            profiles,
            presets,
            b_frames,
            b_frames_note: (most == 0).then_some("This encoder makes no B-frames."),
            padding_available: options.cbr_padding,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn gpu() -> EncoderOptions {
        EncoderOptions {
            rate_controls: ["cbr", "cbr_padded", "vbr", "constant_quality"]
                .map(str::to_owned)
                .to_vec(),
            h264_profiles: ["baseline", "high", "main"].map(str::to_owned).to_vec(),
            b_frames_max: 0,
            cbr_padding: true,
        }
    }

    #[test]
    fn the_defaults_on_a_full_encoder() {
        let f = EncoderForm::new(&RecordProfile::new(), &gpu());
        assert_eq!(
            f.rate_modes.labels().collect::<Vec<_>>(),
            ["CBR", "VBR", "Constant quality"]
        );
        assert_eq!(f.rate_modes.index, 0);
        assert_eq!(f.bitrate_label, "Video bitrate");
        assert_eq!(f.peaks.index, 0, "automatic");
        assert_eq!(
            f.peaks.items[0].0,
            "Automatic (18000 kbit/s, 1.5 x the target bitrate)"
        );
        assert!(f.peaks.items[1..].iter().all(|(_, k)| k.unwrap() >= 12_000));
        assert_eq!(f.qualities.items[f.qualities.index].1, 23);
        assert_eq!(f.keyframes.items[f.keyframes.index].1, 2);
        assert_eq!(
            f.profiles.labels().collect::<Vec<_>>(),
            ["Automatic", "High", "Main", "Baseline"]
        );
        assert_eq!(f.presets.items[f.presets.index].1, Preset::Balanced);
        assert_eq!(f.b_frames.labels().collect::<Vec<_>>(), ["0"]);
        assert!(f.b_frames_note.is_some());
        assert!(f.padding_available);
    }

    #[test]
    fn only_what_the_encoder_has_is_offered() {
        let mut o = gpu();
        o.rate_controls = vec!["cbr".into(), "vbr".into()];
        o.h264_profiles = vec!["main".into()];
        o.b_frames_max = 2;
        o.cbr_padding = false;
        let f = EncoderForm::new(&RecordProfile::new(), &o);
        assert_eq!(f.rate_modes.labels().collect::<Vec<_>>(), ["CBR", "VBR"]);
        assert_eq!(
            f.profiles.labels().collect::<Vec<_>>(),
            ["Automatic", "Main"]
        );
        assert_eq!(f.b_frames.labels().collect::<Vec<_>>(), ["0", "1", "2"]);
        assert_eq!(f.b_frames_note, None);
        assert!(!f.padding_available);
    }

    #[test]
    fn saved_values_the_encoder_lacks_are_still_shown() {
        let mut p = RecordProfile::new();
        p.rate_control = RateMode::ConstantQuality;
        p.profile = Some("baseline".into());
        p.b_frames = 3;
        p.quality = 30;
        p.keyframe_seconds = 7;
        let mut o = gpu();
        o.rate_controls = vec!["cbr".into()];
        o.h264_profiles = vec!["high".into()];
        let f = EncoderForm::new(&p, &o);
        assert_eq!(
            f.rate_modes.get(f.rate_modes.index as i32),
            Some(RateMode::ConstantQuality)
        );
        assert_eq!(f.profiles.items[f.profiles.index].0, "Baseline");
        assert_eq!(
            f.b_frames.items[f.b_frames.index].0,
            "3 (this encoder uses 0)"
        );
        assert_eq!(f.qualities.items[f.qualities.index].1, 30);
        assert_eq!(f.keyframes.items[f.keyframes.index].0, "7 s");
        assert_eq!(f.rate_modes.get(9), None);
        assert_eq!(f.rate_modes.get(-1), None);
    }

    #[test]
    fn vbr_names_the_target_and_offers_peaks_above_it() {
        let mut p = RecordProfile::new();
        p.rate_control = RateMode::Vbr;
        p.video_kbps = 20_000;
        p.max_video_kbps = Some(40_000);
        let f = EncoderForm::new(&p, &gpu());
        assert_eq!(f.bitrate_label, "Target bitrate");
        assert_eq!(f.peaks.get(f.peaks.index as i32), Some(Some(40_000)));
        assert!(f.peaks.items[1..].iter().all(|(_, k)| k.unwrap() >= 20_000));
    }

    #[test]
    fn no_encoder_info_offers_every_mode() {
        let f = EncoderForm::new(&RecordProfile::new(), &EncoderOptions::default());
        assert_eq!(f.rate_modes.items.len(), 3);
        assert_eq!(f.profiles.items.len(), 1);
    }
}
