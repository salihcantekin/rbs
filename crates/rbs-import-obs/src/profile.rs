//! The OBS profile's output, stream, replay and audio settings as neutral facts
//! (ADR-0131 decision 4), and the stream key (decision 5).
//!
//! OBS writes two output modes: `[Output] Mode=Simple` uses `[SimpleOutput]`,
//! `Advanced` uses `[AdvOut]` and the encoder JSON files. Encoder ids map, through a
//! data table, to a codec and a hardware-or-software preference, never to an RBS
//! adapter (P3). The composition root maps the facts onto its own settings.

use std::fmt;
use std::path::PathBuf;

use serde_json::Value;

use crate::ini::Ini;
use crate::report::{Area, ImportReport, Outcome, ReportEntry};
use crate::{Options, ProfileFiles};

/// A video codec.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// H.264.
    H264,
    /// HEVC (H.265).
    Hevc,
    /// AV1.
    Av1,
}

/// Where the OBS encoder ran: RBS picks its own encoder of that kind.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderKind {
    /// A GPU or media engine encoder.
    Hardware,
    /// A CPU encoder.
    Software,
}

/// How the encoder spends bits.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    /// Constant bitrate.
    Cbr,
    /// Variable bitrate around a target.
    Vbr,
    /// Constant quality (CRF, CQP, ICQ).
    ConstantQuality,
}

/// Speed versus quality.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeedPreset {
    /// Slower, better.
    Quality,
    /// The middle.
    Balanced,
    /// Faster.
    Speed,
}

/// One encoder's settings. `None` is "not in the profile": the RBS default applies.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EncoderFacts {
    /// Codec and kind, when the OBS encoder id is in the table.
    pub encoder: Option<(Codec, EncoderKind)>,
    /// Rate control.
    pub rate_control: Option<RateControl>,
    /// Video bitrate in kbit/s.
    pub video_kbps: Option<u32>,
    /// VBR peak in kbit/s.
    pub max_video_kbps: Option<u32>,
    /// Constant-quality level, 0 (best) ..= 51.
    pub quality: Option<u8>,
    /// Seconds between keyframes (0 in OBS means automatic: `None`).
    pub keyframe_seconds: Option<u32>,
    /// Speed versus quality.
    pub preset: Option<SpeedPreset>,
    /// Most B-frames.
    pub b_frames: Option<u32>,
    /// Audio bitrate in kbit/s.
    pub audio_kbps: Option<u32>,
}

/// A recording container.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordFormat {
    /// Matroska.
    Mkv,
    /// MP4 (also OBS's fragmented and hybrid MP4).
    Mp4,
    /// QuickTime (also fragmented MOV).
    Mov,
}

/// OBS's output mode.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// `[SimpleOutput]`.
    Simple,
    /// `[AdvOut]` and the encoder JSON files.
    Advanced,
}

/// Recording settings.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingFacts {
    /// The folder OBS records to. Kept only in the plan, never in the report.
    pub folder: Option<PathBuf>,
    /// The container, when RBS has it.
    pub format: Option<RecordFormat>,
    /// The encoder.
    pub encoder: EncoderFacts,
}

/// Stream settings. The key is in [`ImportedSecrets`], never here.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFacts {
    /// The server URL, only when it is `rtmp://` or `srt://` without a query or user
    /// part; the caller still runs its own server check before writing it.
    pub server: Option<String>,
    /// OBS's service name for `rtmp_common` (Twitch, YouTube, ...).
    pub service: Option<String>,
    /// The encoder.
    pub encoder: EncoderFacts,
}

/// Replay buffer settings.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayFacts {
    /// OBS had the replay buffer on.
    pub enabled: bool,
    /// Seconds kept.
    pub seconds: u32,
}

/// Audio settings.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFacts {
    /// `[Audio] SampleRate`.
    pub sample_rate: Option<u32>,
    /// `[Audio] MonitoringDeviceName`; OBS's "Default" is `None`.
    pub monitoring_device: Option<String>,
}

/// The output parts of a profile; each is `None` when the user left it out.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFacts {
    /// The mode OBS used.
    pub mode: OutputMode,
    /// `[Video] OutputCX`/`OutputCY` when it differs from the canvas.
    pub output_size: Option<(u32, u32)>,
    /// Recording.
    pub recording: Option<RecordingFacts>,
    /// Stream.
    pub stream: Option<StreamFacts>,
    /// Replay buffer.
    pub replay: Option<ReplayFacts>,
    /// Audio.
    pub audio: Option<AudioFacts>,
}

/// A stream key. Its `Debug` prints `<redacted>`; it has no `Display` and no
/// `Serialize`, so it reaches only the credential store (ADR-0131 decision 5).
#[derive(Clone, PartialEq, Eq)]
pub struct StreamKey(String);

impl StreamKey {
    /// The key, for the credential store only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for StreamKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The secrets an import carries: only the stream key.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportedSecrets {
    /// `service.json` `settings.key`, when chosen and not empty.
    pub stream_key: Option<StreamKey>,
}

/// OBS encoder ids, simple-mode names and advanced ids, to a codec and a kind.
const ENCODERS: &[(&str, Codec, EncoderKind)] = &[
    // Simple mode.
    ("x264", Codec::H264, EncoderKind::Software),
    ("x264_lowcpu", Codec::H264, EncoderKind::Software),
    ("nvenc", Codec::H264, EncoderKind::Hardware),
    ("nvenc_hevc", Codec::Hevc, EncoderKind::Hardware),
    ("nvenc_av1", Codec::Av1, EncoderKind::Hardware),
    ("qsv", Codec::H264, EncoderKind::Hardware),
    ("qsv_hevc", Codec::Hevc, EncoderKind::Hardware),
    ("qsv_av1", Codec::Av1, EncoderKind::Hardware),
    ("amd", Codec::H264, EncoderKind::Hardware),
    ("amd_hevc", Codec::Hevc, EncoderKind::Hardware),
    ("amd_av1", Codec::Av1, EncoderKind::Hardware),
    ("apple_h264", Codec::H264, EncoderKind::Hardware),
    ("apple_hevc", Codec::Hevc, EncoderKind::Hardware),
    ("svt_av1", Codec::Av1, EncoderKind::Software),
    ("aom_av1", Codec::Av1, EncoderKind::Software),
    // Advanced mode.
    ("obs_x264", Codec::H264, EncoderKind::Software),
    ("ffmpeg_nvenc", Codec::H264, EncoderKind::Hardware),
    ("jim_nvenc", Codec::H264, EncoderKind::Hardware),
    ("obs_nvenc_h264_tex", Codec::H264, EncoderKind::Hardware),
    ("obs_nvenc_h264_soft", Codec::H264, EncoderKind::Hardware),
    ("ffmpeg_hevc_nvenc", Codec::Hevc, EncoderKind::Hardware),
    ("jim_hevc_nvenc", Codec::Hevc, EncoderKind::Hardware),
    ("obs_nvenc_hevc_tex", Codec::Hevc, EncoderKind::Hardware),
    ("obs_nvenc_hevc_soft", Codec::Hevc, EncoderKind::Hardware),
    ("jim_av1_nvenc", Codec::Av1, EncoderKind::Hardware),
    ("obs_nvenc_av1_tex", Codec::Av1, EncoderKind::Hardware),
    ("obs_nvenc_av1_soft", Codec::Av1, EncoderKind::Hardware),
    ("obs_qsv11", Codec::H264, EncoderKind::Hardware),
    ("obs_qsv11_v2", Codec::H264, EncoderKind::Hardware),
    ("obs_qsv11_soft", Codec::H264, EncoderKind::Hardware),
    ("obs_qsv11_soft_v2", Codec::H264, EncoderKind::Hardware),
    ("obs_qsv11_hevc", Codec::Hevc, EncoderKind::Hardware),
    ("obs_qsv11_hevc_soft", Codec::Hevc, EncoderKind::Hardware),
    ("obs_qsv11_av1", Codec::Av1, EncoderKind::Hardware),
    ("obs_qsv11_av1_soft", Codec::Av1, EncoderKind::Hardware),
    ("amd_amf_h264", Codec::H264, EncoderKind::Hardware),
    ("h264_texture_amf", Codec::H264, EncoderKind::Hardware),
    ("h265_texture_amf", Codec::Hevc, EncoderKind::Hardware),
    ("av1_texture_amf", Codec::Av1, EncoderKind::Hardware),
    (
        "com.apple.videotoolbox.videoencoder.ave.avc",
        Codec::H264,
        EncoderKind::Hardware,
    ),
    (
        "com.apple.videotoolbox.videoencoder.h264.gva",
        Codec::H264,
        EncoderKind::Hardware,
    ),
    (
        "com.apple.videotoolbox.videoencoder.ave.hevc",
        Codec::Hevc,
        EncoderKind::Hardware,
    ),
    ("ffmpeg_vaapi", Codec::H264, EncoderKind::Hardware),
    ("ffmpeg_vaapi_tex", Codec::H264, EncoderKind::Hardware),
    ("obs_ffmpeg_vaapi", Codec::H264, EncoderKind::Hardware),
    ("hevc_ffmpeg_vaapi", Codec::Hevc, EncoderKind::Hardware),
    ("hevc_ffmpeg_vaapi_tex", Codec::Hevc, EncoderKind::Hardware),
    ("av1_ffmpeg_vaapi", Codec::Av1, EncoderKind::Hardware),
    ("av1_ffmpeg_vaapi_tex", Codec::Av1, EncoderKind::Hardware),
    ("ffmpeg_svt_av1", Codec::Av1, EncoderKind::Software),
    ("ffmpeg_aom_av1", Codec::Av1, EncoderKind::Software),
];

/// The codec and kind of an OBS encoder id, from the table.
pub fn encoder_of(obs_id: &str) -> Option<(Codec, EncoderKind)> {
    ENCODERS
        .iter()
        .find(|(id, ..)| *id == obs_id)
        .map(|&(_, c, k)| (c, k))
}

fn preset_of(text: &str) -> Option<SpeedPreset> {
    Some(match text.to_ascii_lowercase().as_str() {
        "ultrafast" | "superfast" | "veryfast" | "faster" | "fast" | "p1" | "p2" | "p3"
        | "performance" | "hp" | "ll" | "llhp" | "speed" => SpeedPreset::Speed,
        "medium" | "p4" | "p5" | "default" | "balanced" => SpeedPreset::Balanced,
        "slow" | "slower" | "veryslow" | "placebo" | "p6" | "p7" | "quality" | "hq" | "llhq"
        | "max_quality" | "bd" => SpeedPreset::Quality,
        _ => return None,
    })
}

fn setting(area: Area, name: &str, outcome: Outcome, reason: impl Into<String>) -> ReportEntry {
    ReportEntry::new(outcome, area, name, "", reason)
}

fn json_u32(v: &Value, key: &str) -> Option<u32> {
    let v = v.get(key)?;
    v.as_u64()
        .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f.round() as u64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .and_then(|n| u32::try_from(n).ok())
}

/// Reads an advanced-mode encoder JSON (`streamEncoder.json`, `recordEncoder.json`).
fn advanced_encoder(
    id: &str,
    json: Option<&[u8]>,
    what: &str,
    report: &mut ImportReport,
) -> EncoderFacts {
    let mut e = EncoderFacts {
        encoder: encoder_of(id),
        ..EncoderFacts::default()
    };
    if e.encoder.is_none() && !id.is_empty() {
        report.push(
            setting(
                Area::Profile,
                what,
                Outcome::Approximated,
                format!(
                    "The OBS encoder {id:?} is not known to RBS, so RBS picks the best encoder it has."
                ),
            )
            .with_hint("Check the encoder in Settings."),
        );
    }
    let Some(v) = json.and_then(|b| serde_json::from_slice::<Value>(b).ok()) else {
        return e;
    };
    let rc = v
        .get("rate_control")
        .and_then(Value::as_str)
        .unwrap_or("CBR")
        .to_ascii_uppercase();
    e.rate_control = match rc.as_str() {
        "CBR" => Some(RateControl::Cbr),
        "VBR" | "ABR" | "CQVBR" => Some(RateControl::Vbr),
        "CRF" | "CQP" | "ICQ" | "CQ" | "QVBR" => Some(RateControl::ConstantQuality),
        other => {
            report.push(setting(
                Area::Profile,
                what,
                Outcome::Approximated,
                format!("The rate control {other:?} is not in RBS, so constant bitrate is used."),
            ));
            Some(RateControl::Cbr)
        }
    };
    if rc == "CQVBR" || rc == "ABR" {
        report.push(setting(
            Area::Profile,
            what,
            Outcome::Approximated,
            format!("The rate control {rc:?} became variable bitrate."),
        ));
    }
    e.video_kbps = json_u32(&v, "bitrate").filter(|n| *n > 0);
    e.max_video_kbps = json_u32(&v, "max_bitrate").filter(|n| *n > 0);
    e.quality = ["crf", "cqp", "icq_quality", "cq_level", "qp"]
        .iter()
        .find_map(|k| json_u32(&v, k))
        .map(|q| q.min(51) as u8);
    e.keyframe_seconds = json_u32(&v, "keyint_sec").filter(|n| *n > 0);
    e.preset = ["preset2", "preset"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str).and_then(preset_of));
    e.b_frames = json_u32(&v, "bf");
    e
}

fn simple_encoder(id: &str, what: &str, report: &mut ImportReport) -> EncoderFacts {
    let e = EncoderFacts {
        encoder: encoder_of(id),
        ..EncoderFacts::default()
    };
    if e.encoder.is_none() && !id.is_empty() {
        report.push(setting(
            Area::Profile,
            what,
            Outcome::Approximated,
            format!(
                "The OBS encoder {id:?} is not known to RBS, so RBS picks the best encoder it has."
            ),
        ));
    }
    e
}

fn record_format(text: &str, report: &mut ImportReport) -> Option<RecordFormat> {
    let f = match text.trim().to_ascii_lowercase().as_str() {
        "" => return None,
        "mkv" => RecordFormat::Mkv,
        "mp4" | "fragmented_mp4" | "hybrid_mp4" => RecordFormat::Mp4,
        "mov" | "fragmented_mov" | "hybrid_mov" => RecordFormat::Mov,
        other => {
            report.push(
                setting(
                    Area::Profile,
                    "Recording format",
                    Outcome::Approximated,
                    format!("RBS does not record {other} files, so the recording uses MKV."),
                )
                .with_hint("MKV keeps the recording when the computer stops suddenly."),
            );
            return Some(RecordFormat::Mkv);
        }
    };
    if text.contains("fragmented") || text.contains("hybrid") {
        report.push(setting(
            Area::Profile,
            "Recording format",
            Outcome::Approximated,
            "OBS's fragmented or hybrid file became a plain file of that container.",
        ));
    }
    Some(f)
}

/// Reads the profile's output facts and the stream key.
pub(crate) fn read(
    ini: &Ini,
    files: &ProfileFiles,
    canvas: Option<(u32, u32)>,
    output: Option<(u32, u32)>,
    options: &Options,
    report: &mut ImportReport,
) -> (OutputFacts, ImportedSecrets) {
    let mode = match ini.get("Output", "Mode") {
        Some(m) if m.eq_ignore_ascii_case("advanced") => OutputMode::Advanced,
        _ => OutputMode::Simple,
    };
    let flag = |s: &str, k: &str| {
        ini.get(s, k)
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
    };
    let mut secrets = ImportedSecrets::default();

    let (stream_enc, rec_enc, folder, format, replay) = match mode {
        OutputMode::Simple => {
            let sec = "SimpleOutput";
            let mut stream = simple_encoder(
                ini.get(sec, "StreamEncoder").unwrap_or("x264"),
                "Stream encoder",
                report,
            );
            stream.rate_control = Some(RateControl::Cbr);
            stream.video_kbps = ini.parse_as(sec, "VBitrate");
            stream.audio_kbps = ini.parse_as(sec, "ABitrate");
            stream.preset = ini
                .get(sec, "Preset")
                .or_else(|| ini.get(sec, "NVENCPreset2"))
                .and_then(preset_of);
            let quality = ini.get(sec, "RecQuality").unwrap_or("Stream");
            let rec = if quality.eq_ignore_ascii_case("Stream") {
                stream
            } else {
                let mut r = simple_encoder(
                    ini.get(sec, "RecEncoder").unwrap_or("x264"),
                    "Recording encoder",
                    report,
                );
                r.rate_control = Some(RateControl::ConstantQuality);
                r.audio_kbps = stream.audio_kbps;
                let (q, reason) = match quality {
                    "Small" | "HQ" => {
                        let q = if quality == "Small" { 23 } else { 16 };
                        (
                            q,
                            format!(
                                "OBS's {quality:?} recording quality became constant quality {q}."
                            ),
                        )
                    }
                    _ => (
                        16,
                        format!(
                            "The recording quality {quality:?} is not in RBS, so constant quality 16 is used."
                        ),
                    ),
                };
                r.quality = Some(q);
                report.push(setting(
                    Area::Profile,
                    "Recording quality",
                    Outcome::Approximated,
                    reason,
                ));
                r
            };
            let format = ini
                .get(sec, "RecFormat2")
                .or_else(|| ini.get(sec, "RecFormat"))
                .unwrap_or("");
            (
                stream,
                rec,
                ini.get(sec, "FilePath"),
                format.to_owned(),
                (flag(sec, "RecRB"), ini.parse_as::<u32>(sec, "RecRBTime")),
            )
        }
        OutputMode::Advanced => {
            let sec = "AdvOut";
            let track = ini
                .parse_as::<u32>(sec, "TrackIndex")
                .unwrap_or(1)
                .clamp(1, 6);
            let audio = ini.parse_as(sec, &format!("Track{track}Bitrate"));
            let stream_id = ini.get(sec, "Encoder").unwrap_or("obs_x264");
            let mut stream = advanced_encoder(
                stream_id,
                files.stream_encoder.as_deref(),
                "Stream encoder",
                report,
            );
            stream.audio_kbps = audio;
            let rec_id = ini.get(sec, "RecEncoder").unwrap_or("none");
            let mut rec = if rec_id.eq_ignore_ascii_case("none") {
                stream
            } else {
                advanced_encoder(
                    rec_id,
                    files.record_encoder.as_deref(),
                    "Recording encoder",
                    report,
                )
            };
            let rec_tracks = ini.parse_as::<u32>(sec, "RecTracks").unwrap_or(1);
            let first = (0..6).find(|i| rec_tracks & (1 << i) != 0).unwrap_or(0) + 1;
            rec.audio_kbps = ini.parse_as(sec, &format!("Track{first}Bitrate"));
            if rec_tracks.count_ones() > 1 {
                report.push(setting(
                    Area::Profile,
                    "Recording audio tracks",
                    Outcome::Approximated,
                    "OBS recorded several audio tracks; RBS records the mix on one track.",
                ));
            }
            if ini.get(sec, "RecType") == Some("1") {
                report.push(setting(
                    Area::Profile,
                    "Custom output (FFmpeg)",
                    Outcome::Skipped,
                    "OBS's custom FFmpeg output is not in RBS, so the standard recording settings are used.",
                ));
            }
            let format = ini
                .get(sec, "RecFormat2")
                .or_else(|| ini.get(sec, "RecFormat"))
                .unwrap_or("");
            (
                stream,
                rec,
                ini.get(sec, "RecFilePath"),
                format.to_owned(),
                (flag(sec, "RecRB"), ini.parse_as::<u32>(sec, "RecRBTime")),
            )
        }
    };

    let recording = options.recording.then(|| RecordingFacts {
        folder: folder.filter(|f| !f.trim().is_empty()).map(PathBuf::from),
        format: record_format(&format, report),
        encoder: rec_enc,
    });
    if !options.recording {
        report.push(setting(
            Area::Profile,
            "Recording settings",
            Outcome::NotChosen,
            "Recording settings were not chosen.",
        ));
    }

    let stream = if options.stream_server {
        Some(read_service(
            files,
            stream_enc,
            options,
            &mut secrets,
            report,
        ))
    } else {
        report.push(setting(
            Area::Profile,
            "Stream settings",
            Outcome::NotChosen,
            "Stream settings were not chosen.",
        ));
        if options.stream_key {
            // The key belongs to the stream settings: without them it has nowhere to go.
            report.push(setting(
                Area::Profile,
                "Stream key",
                Outcome::NotChosen,
                "The stream key comes only with the stream settings.",
            ));
        }
        None
    };

    let replay = if options.replay {
        Some(ReplayFacts {
            enabled: replay.0,
            seconds: replay.1.filter(|s| *s > 0).unwrap_or(20),
        })
    } else {
        report.push(setting(
            Area::Profile,
            "Replay buffer",
            Outcome::NotChosen,
            "Replay buffer settings were not chosen.",
        ));
        None
    };

    let audio = if options.audio_settings {
        let device = ini
            .get("Audio", "MonitoringDeviceName")
            .filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case("default"))
            .map(str::to_owned);
        if ini
            .get("Audio", "ChannelSetup")
            .is_some_and(|c| c != "Stereo")
        {
            report.push(setting(
                Area::Setting,
                "Audio channels",
                Outcome::Approximated,
                "RBS mixes in stereo.",
            ));
        }
        Some(AudioFacts {
            sample_rate: ini.parse_as("Audio", "SampleRate"),
            monitoring_device: device,
        })
    } else {
        report.push(setting(
            Area::Setting,
            "Audio settings",
            Outcome::NotChosen,
            "Audio settings were not chosen.",
        ));
        None
    };

    let output_size = output.filter(|o| Some(*o) != canvas && o.0 > 0 && o.1 > 0);
    (
        OutputFacts {
            mode,
            output_size,
            recording,
            stream,
            replay,
            audio,
        },
        secrets,
    )
}

/// The server is kept only as `rtmp://` or `srt://` without a query or user part;
/// anything else is reported by its scheme only (a query may hold a secret).
fn safe_server(url: &str) -> Result<String, String> {
    let url = url.trim();
    let scheme = url
        .split_once("://")
        .map(|(s, _)| s.to_ascii_lowercase())
        .unwrap_or_default();
    let ok = matches!(scheme.as_str(), "rtmp" | "srt") && !url.contains(['?', '#', '@', ' ']);
    if ok { Ok(url.to_owned()) } else { Err(scheme) }
}

fn read_service(
    files: &ProfileFiles,
    mut encoder: EncoderFacts,
    options: &Options,
    secrets: &mut ImportedSecrets,
    report: &mut ImportReport,
) -> StreamFacts {
    let v = files
        .service_json
        .as_deref()
        .and_then(|b| serde_json::from_slice::<Value>(b).ok())
        .unwrap_or(Value::Null);
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    let settings = v.get("settings").cloned().unwrap_or(Value::Null);
    let text = |k: &str| {
        settings
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    if encoder.video_kbps.is_none() {
        encoder.video_kbps = json_u32(&settings, "bitrate");
    }
    let mut facts = StreamFacts {
        server: None,
        service: None,
        encoder,
    };
    match kind {
        "rtmp_custom" | "rtmp_common" => {
            if kind == "rtmp_common" {
                facts.service = text("service").map(str::to_owned);
            }
            match text("server") {
                None => {}
                Some(s) if s.eq_ignore_ascii_case("auto") => report.push(
                    setting(
                        Area::Profile,
                        "Stream server",
                        Outcome::Skipped,
                        "OBS picked the server automatically, which RBS does not do yet.",
                    )
                    .with_hint("Enter the server address in Settings › Stream."),
                ),
                Some(s) => match safe_server(s) {
                    Ok(url) => facts.server = Some(url),
                    Err(scheme) => report.push(
                        setting(
                            Area::Profile,
                            "Stream server",
                            Outcome::Skipped,
                            if scheme.is_empty() {
                                "The server address is not one RBS can use.".to_owned()
                            } else {
                                format!(
                                    "The {scheme}:// server address is not one RBS can use yet, or it carries options that may hold a secret."
                                )
                            },
                        )
                        .with_hint("Enter the server address in Settings › Stream."),
                    ),
                },
            }
            for (k, label) in [
                ("username", "user name"),
                ("password", "password"),
                ("bearer_token", "bearer token"),
            ] {
                if text(k).is_some() {
                    report.push(setting(
                        Area::Profile,
                        "Stream sign-in",
                        Outcome::Skipped,
                        format!("The server's {label} is not in RBS and was not read."),
                    ));
                }
            }
            if options.stream_key {
                secrets.stream_key = text("key").map(|k| StreamKey(k.to_owned()));
            } else if text("key").is_some() {
                report.push(setting(
                    Area::Profile,
                    "Stream key",
                    Outcome::NotChosen,
                    "The stream key was not chosen.",
                ));
            }
        }
        "" => {}
        other => report.push(setting(
            Area::Profile,
            "Stream service",
            Outcome::Skipped,
            format!("The OBS stream service type {other:?} is not in RBS yet."),
        )),
    }
    facts
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn the_key_never_prints() {
        let k = StreamKey("live_123".into());
        assert_eq!(format!("{k:?}"), "<redacted>");
        let s = ImportedSecrets {
            stream_key: Some(k),
        };
        assert!(!format!("{s:?}").contains("live_123"));
    }

    #[test]
    fn servers_with_options_or_other_schemes_are_dropped() {
        assert!(safe_server("rtmp://live.example.com/app").is_ok());
        assert!(safe_server("srt://example.com:9000").is_ok());
        assert_eq!(
            safe_server("srt://example.com:9000?streamid=SECRET"),
            Err("srt".into())
        );
        assert_eq!(safe_server("rtmps://x/app"), Err("rtmps".into()));
        assert_eq!(safe_server("rtmp://user:pw@x/app"), Err("rtmp".into()));
        assert_eq!(safe_server("nonsense"), Err(String::new()));
    }

    #[test]
    fn encoder_ids_map_to_codec_and_kind() {
        assert_eq!(
            encoder_of("jim_hevc_nvenc"),
            Some((Codec::Hevc, EncoderKind::Hardware))
        );
        assert_eq!(
            encoder_of("obs_x264"),
            Some((Codec::H264, EncoderKind::Software))
        );
        assert_eq!(encoder_of("mystery"), None);
        assert_eq!(preset_of("veryfast"), Some(SpeedPreset::Speed));
        assert_eq!(preset_of("p5"), Some(SpeedPreset::Balanced));
        assert_eq!(preset_of("p7"), Some(SpeedPreset::Quality));
    }
}
