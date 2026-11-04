//! Command-line parsing (no dependency: the grammar is small).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rbs_codec::{EncoderPreset, KeyframeInterval, RateControl};
use rbs_core::media::{ContainerId, Size};
use rbs_core::timing::FrameRate;
use rbs_remote_settings::Access;

/// What the user asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `rbs-cli record ...`
    Record(Box<RecordArgs>),
    /// `rbs-cli devices`: list adapters and capture targets.
    Devices,
    /// `rbs-cli token ...`: manage remote control access tokens.
    Token(TokenCommand),
    /// `rbs-cli help` or `--help`.
    Help,
}

/// `rbs-cli token ...` (ADR-0044 decision 9): the access tokens apps use to control
/// RBS, in the same settings file as the desktop app's Settings, Remote control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenCommand {
    /// `token list`.
    List,
    /// `token create <name> [--access view|control|full]`.
    Create {
        /// The token's name.
        name: String,
        /// What its app may do.
        access: Access,
    },
    /// `token revoke <name>`.
    Revoke {
        /// The token's name.
        name: String,
    },
}

/// Options of `record`.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordArgs {
    /// The file written.
    pub out: PathBuf,
    /// The container, from the file's extension.
    pub container: ContainerId,
    /// Stop after this long; `None` records until Enter is pressed.
    pub duration: Option<Duration>,
    /// Canvas size.
    pub size: Size,
    /// Canvas frame rate.
    pub fps: FrameRate,
    /// Video rate control, from `--rate-control`, `--bitrate`, `--max-bitrate` and
    /// `--quality`.
    pub rate_control: RateControl,
    /// Keyframe interval.
    pub keyframe_interval: KeyframeInterval,
    /// Most B-frames between references.
    pub b_frames: u32,
    /// Codec profile name; `None` lets the encoder pick.
    pub profile: Option<String>,
    /// Encoder speed versus quality.
    pub preset: EncoderPreset,
    /// Audio bitrate in bit/s.
    pub audio_bitrate: u64,
    /// Video encoder adapter id, or its last dotted part (`nvenc`): only this one is used.
    pub encoder: Option<String>,
    /// Display index in the display list (`devices`), default the first.
    pub display: Option<usize>,
    /// Camera index in the camera list, default the first.
    pub camera: Option<usize>,
    /// Leave the camera out.
    pub no_camera: bool,
    /// Leave the microphone out.
    pub no_mic: bool,
    /// Leave the system audio out.
    pub no_system_audio: bool,
    /// Record no audio track.
    pub no_audio: bool,
    /// Microphone sync offset in ms: positive delays it, negative advances it.
    pub mic_offset_ms: i32,
    /// System audio sync offset in ms: positive delays it, negative advances it.
    pub system_audio_offset_ms: i32,
    /// Replace an existing file.
    pub overwrite: bool,
}

/// The usage text.
pub const USAGE: &str = "\
usage:
  rbs-cli record <file.mkv|file.mp4> [options]
  rbs-cli devices
  rbs-cli token list
  rbs-cli token create <name> [--access view|control|full]
  rbs-cli token revoke <name>

record options:
  --seconds N          stop after N seconds (default: press Enter to stop)
  --size WxH           canvas size (default 1920x1080)
  --fps N              canvas frame rate (default 60)
  --rate-control MODE  cbr, vbr or cq (constant quality) (default cbr)
  --bitrate KBPS       video bitrate in kbit/s, the target with vbr (default 12000)
  --max-bitrate KBPS   vbr peak in kbit/s, at least --bitrate (default 1.5 x --bitrate)
  --quality N          cq level, 0 (best) to 51 (default 23)
  --keyframe-interval S
                       seconds between keyframes (default 2)
  --b-frames N         most B-frames, 0 to 4; encoders that have fewer use fewer
                       (default 0)
  --profile NAME       codec profile: baseline, main, high (H.264) (default: the
                       encoder's choice)
  --preset NAME        quality, balanced or speed (default balanced)
  --cbr-padding        with cbr, pad the video to its full bitrate (filler data)
  --audio-bitrate KBPS audio bitrate in kbit/s (default 160)
  --encoder ID         use only this video encoder: an adapter id or its last part (nvenc, amf,
                       videotoolbox)
  --display N          display number from `devices` (default 0)
  --camera N           camera number from `devices` (default 0)
  --no-camera          no camera picture
  --no-mic             no microphone
  --no-system-audio    no system audio
  --no-audio           no audio track at all
  --mic-offset MS      delay the microphone by MS ms, -200..200 (negative plays it
                       earlier, by 35 ms at most)
  --system-audio-offset MS
                       the same for the system audio
  --overwrite          replace the file if it exists

token commands (access tokens for apps that control RBS, as in Settings, Remote
control; the token is printed once, keep it):
  --access LEVEL       view: see state only; control: also change sources and
                       start and stop recording (default, right for panels);
                       full: also change settings";

fn value<'a>(it: &mut impl Iterator<Item = &'a str>, flag: &str) -> Result<&'a str, String> {
    it.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn number<T: std::str::FromStr>(text: &str, flag: &str) -> Result<T, String> {
    text.parse()
        .map_err(|_| format!("{flag}: {text:?} is not a number"))
}

fn positive(text: &str, flag: &str) -> Result<u32, String> {
    match number::<u32>(text, flag)? {
        0 => Err(format!("{flag} must be above 0")),
        n => Ok(n),
    }
}

/// Largest sync offset accepted, in ms (the audio input's FIFO holds this much,
/// `rbs_sources::DEFAULT_MAX_AUDIO_OFFSET`). An advance is further limited by the audio
/// graph's latency; the input clamps it.
pub const MAX_OFFSET_MS: i32 = 200;

fn offset(text: &str, flag: &str) -> Result<i32, String> {
    let ms = number::<i32>(text, flag)?;
    if ms.unsigned_abs() > MAX_OFFSET_MS.unsigned_abs() {
        return Err(format!(
            "{flag}: {ms} ms is more than {MAX_OFFSET_MS} ms either way"
        ));
    }
    Ok(ms)
}

/// Most B-frames `--b-frames` takes.
pub const MAX_B_FRAMES: u32 = 4;

/// Default constant-quality level.
pub const DEFAULT_QUALITY: u8 = 23;

/// The video rate-control flags, combined once every flag is read.
#[derive(Default)]
struct RateFlags {
    mode: Option<String>,
    bitrate_bps: Option<u64>,
    max_bps: Option<u64>,
    quality: Option<u8>,
    padded: bool,
}

impl RateFlags {
    fn rate_control(self) -> Result<RateControl, String> {
        let bitrate = self.bitrate_bps.unwrap_or(12_000_000);
        let mode = self.mode.as_deref().unwrap_or("cbr");
        if mode != "vbr" && self.max_bps.is_some() {
            return Err("--max-bitrate needs --rate-control vbr".into());
        }
        if mode != "cq" && self.quality.is_some() {
            return Err("--quality needs --rate-control cq".into());
        }
        if mode != "cbr" && self.padded {
            return Err("--cbr-padding needs --rate-control cbr".into());
        }
        match mode {
            "cbr" if self.padded => Ok(RateControl::CbrPadded {
                bitrate_bps: bitrate,
            }),
            "cbr" => Ok(RateControl::Cbr {
                bitrate_bps: bitrate,
            }),
            "vbr" => {
                let max_bps = self.max_bps.unwrap_or(bitrate / 2 * 3);
                if max_bps < bitrate {
                    return Err("--max-bitrate must be at least --bitrate".into());
                }
                Ok(RateControl::Vbr {
                    target_bps: bitrate,
                    max_bps,
                })
            }
            "cq" => {
                if self.bitrate_bps.is_some() {
                    return Err("--bitrate does not apply to --rate-control cq".into());
                }
                Ok(RateControl::ConstantQuality {
                    level: self.quality.unwrap_or(DEFAULT_QUALITY),
                })
            }
            other => Err(format!("--rate-control: {other:?} is not cbr, vbr or cq")),
        }
    }
}

fn preset(text: &str) -> Result<EncoderPreset, String> {
    match text {
        "quality" => Ok(EncoderPreset::Quality),
        "balanced" => Ok(EncoderPreset::Balanced),
        "speed" => Ok(EncoderPreset::Speed),
        _ => Err(format!(
            "--preset: {text:?} is not quality, balanced or speed"
        )),
    }
}

fn kbps(text: &str, flag: &str) -> Result<u64, String> {
    Ok(u64::from(positive(text, flag)?) * 1000)
}

fn size(text: &str) -> Result<Size, String> {
    let (w, h) = text
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("--size: {text:?} is not WxH"))?;
    let (w, h) = (positive(w, "--size")?, positive(h, "--size")?);
    if w % 2 != 0 || h % 2 != 0 {
        return Err("--size: width and height must be even (4:2:0 video)".into());
    }
    Ok(Size::new(w, h))
}

/// One line with the video settings, for the log: `cbr 12000 kbit/s, keyframe every
/// 2 s, 0 B-frames, profile auto, preset balanced`.
pub fn describe_video(a: &RecordArgs) -> String {
    let rate = match a.rate_control {
        RateControl::Cbr { bitrate_bps } => format!("cbr {} kbit/s", bitrate_bps / 1000),
        RateControl::CbrPadded { bitrate_bps } => {
            format!("cbr {} kbit/s padded", bitrate_bps / 1000)
        }
        RateControl::Vbr {
            target_bps,
            max_bps,
        } => format!("vbr {} kbit/s, peak {}", target_bps / 1000, max_bps / 1000),
        RateControl::ConstantQuality { level } => format!("cq {level}"),
        _ => "other rate control".into(),
    };
    let keyframes = match a.keyframe_interval {
        KeyframeInterval::Duration(d) => format!("{} s", d.as_secs()),
        KeyframeInterval::Frames(n) => format!("{n} frames"),
        _ => "?".into(),
    };
    let preset = match a.preset {
        EncoderPreset::Quality => "quality",
        EncoderPreset::Speed => "speed",
        _ => "balanced",
    };
    format!(
        "{rate}, keyframe every {keyframes}, {} B-frames at most, profile {}, preset {preset}",
        a.b_frames,
        a.profile.as_deref().unwrap_or("auto")
    )
}

/// The container a file name asks for.
pub fn container_of(path: &Path) -> Result<ContainerId, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("mkv") => Ok(ContainerId::Matroska),
        Some("mp4") => Ok(ContainerId::Mp4Fragmented),
        _ => Err(format!(
            "{}: the file must end in .mkv or .mp4",
            path.display()
        )),
    }
}

/// Parses the arguments after the program name.
pub fn parse<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<Command, String> {
    let mut it = args.into_iter();
    match it.next() {
        None | Some("help" | "--help" | "-h") => Ok(Command::Help),
        Some("devices") => match it.next() {
            None => Ok(Command::Devices),
            Some(extra) => Err(format!("devices: unexpected {extra:?}")),
        },
        Some("record") => record(it).map(|a| Command::Record(Box::new(a))),
        Some("token") => token(it).map(Command::Token),
        Some(other) => Err(format!("unknown command {other:?}")),
    }
}

fn token<'a>(mut it: impl Iterator<Item = &'a str>) -> Result<TokenCommand, String> {
    let command = match it.next() {
        Some("list") => TokenCommand::List,
        Some("create") => {
            let mut name = None;
            let mut access = Access::Control;
            while let Some(arg) = it.next() {
                match arg {
                    "--access" => {
                        let word = value(&mut it, arg)?;
                        access = Access::parse(word).ok_or_else(|| {
                            format!("--access: {word:?} is not view, control or full")
                        })?;
                    }
                    flag if flag.starts_with("--") => {
                        return Err(format!("token create: unknown option {flag:?}"));
                    }
                    n if name.is_none() => name = Some(n.to_owned()),
                    extra => return Err(format!("token create: unexpected {extra:?}")),
                }
            }
            let name = name.ok_or("token create needs a name, such as \"Stream Deck\"")?;
            return Ok(TokenCommand::Create { name, access });
        }
        Some("revoke") => {
            let name = it.next().ok_or("token revoke needs the token's name")?;
            TokenCommand::Revoke {
                name: name.to_owned(),
            }
        }
        Some(other) => return Err(format!("token: unknown command {other:?}")),
        None => return Err("token needs list, create or revoke".into()),
    };
    match it.next() {
        None => Ok(command),
        Some(extra) => Err(format!("token: unexpected {extra:?}")),
    }
}

fn record<'a>(mut it: impl Iterator<Item = &'a str>) -> Result<RecordArgs, String> {
    let mut out = None;
    let mut rate = RateFlags::default();
    let mut a = RecordArgs {
        out: PathBuf::new(),
        container: ContainerId::Matroska,
        duration: None,
        size: Size::new(1920, 1080),
        fps: FrameRate::FPS_60,
        rate_control: RateControl::Cbr {
            bitrate_bps: 12_000_000,
        },
        keyframe_interval: KeyframeInterval::Duration(Duration::from_secs(2)),
        b_frames: 0,
        profile: None,
        preset: EncoderPreset::Balanced,
        audio_bitrate: 160_000,
        encoder: None,
        display: None,
        camera: None,
        no_camera: false,
        no_mic: false,
        no_system_audio: false,
        no_audio: false,
        mic_offset_ms: 0,
        system_audio_offset_ms: 0,
        overwrite: false,
    };
    while let Some(arg) = it.next() {
        match arg {
            "--seconds" => {
                let s = positive(value(&mut it, arg)?, arg)?;
                a.duration = Some(Duration::from_secs(u64::from(s)));
            }
            "--size" => a.size = size(value(&mut it, arg)?)?,
            "--fps" => {
                let n = positive(value(&mut it, arg)?, arg)?;
                a.fps = FrameRate::new(n, 1).ok_or("--fps: not a frame rate")?;
            }
            "--rate-control" => rate.mode = Some(value(&mut it, arg)?.to_owned()),
            "--bitrate" => rate.bitrate_bps = Some(kbps(value(&mut it, arg)?, arg)?),
            "--max-bitrate" => rate.max_bps = Some(kbps(value(&mut it, arg)?, arg)?),
            "--quality" => {
                let level = number::<u8>(value(&mut it, arg)?, arg)?;
                if level > 51 {
                    return Err(format!("{arg}: {level} is above 51"));
                }
                rate.quality = Some(level);
            }
            "--keyframe-interval" => {
                let s = positive(value(&mut it, arg)?, arg)?;
                a.keyframe_interval = KeyframeInterval::Duration(Duration::from_secs(u64::from(s)));
            }
            "--b-frames" => {
                let n = number::<u32>(value(&mut it, arg)?, arg)?;
                if n > MAX_B_FRAMES {
                    return Err(format!("{arg}: {n} is above {MAX_B_FRAMES}"));
                }
                a.b_frames = n;
            }
            "--profile" => {
                let name = value(&mut it, arg)?.to_ascii_lowercase();
                a.profile = Some(name);
            }
            "--preset" => a.preset = preset(value(&mut it, arg)?)?,
            "--cbr-padding" => rate.padded = true,
            "--audio-bitrate" => a.audio_bitrate = kbps(value(&mut it, arg)?, arg)?,
            "--encoder" => a.encoder = Some(value(&mut it, arg)?.to_owned()),
            "--display" => a.display = Some(number(value(&mut it, arg)?, arg)?),
            "--camera" => a.camera = Some(number(value(&mut it, arg)?, arg)?),
            "--no-camera" => a.no_camera = true,
            "--no-mic" => a.no_mic = true,
            "--no-system-audio" => a.no_system_audio = true,
            "--no-audio" => a.no_audio = true,
            "--mic-offset" => a.mic_offset_ms = offset(value(&mut it, arg)?, arg)?,
            "--system-audio-offset" => {
                a.system_audio_offset_ms = offset(value(&mut it, arg)?, arg)?;
            }
            "--overwrite" => a.overwrite = true,
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            path if out.is_none() => out = Some(PathBuf::from(path)),
            extra => return Err(format!("unexpected argument {extra:?}")),
        }
    }
    a.out = out.ok_or("record: the output file is missing")?;
    a.container = container_of(&a.out)?;
    a.rate_control = rate.rate_control()?;
    Ok(a)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn parse_record(args: &[&str]) -> Result<RecordArgs, String> {
        match parse(std::iter::once("record").chain(args.iter().copied()))? {
            Command::Record(r) => Ok(*r),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn record_defaults_and_options() {
        let a = parse_record(&["out.mkv"]).unwrap();
        assert_eq!(a.container, ContainerId::Matroska);
        assert_eq!(a.size, Size::new(1920, 1080));
        assert_eq!(a.fps, FrameRate::FPS_60);
        assert_eq!(a.duration, None);
        assert!(!a.no_camera && !a.no_audio);

        let a = parse_record(&[
            "--seconds",
            "30",
            "D:\\rec\\a.MP4",
            "--size",
            "1280x720",
            "--fps",
            "30",
            "--bitrate",
            "8000",
            "--audio-bitrate",
            "192",
            "--encoder",
            "amf",
            "--display",
            "1",
            "--no-camera",
            "--no-system-audio",
            "--mic-offset",
            "-30",
            "--system-audio-offset",
            "25",
        ])
        .unwrap();
        assert_eq!(a.container, ContainerId::Mp4Fragmented);
        assert_eq!(a.duration, Some(Duration::from_secs(30)));
        assert_eq!(a.size, Size::new(1280, 720));
        assert_eq!(a.fps, FrameRate::FPS_30);
        assert_eq!(
            a.rate_control,
            RateControl::Cbr {
                bitrate_bps: 8_000_000
            }
        );
        assert_eq!(a.audio_bitrate, 192_000);
        assert_eq!(a.encoder.as_deref(), Some("amf"));
        assert_eq!(a.display, Some(1));
        assert!(a.no_camera && a.no_system_audio && !a.no_mic);
        assert_eq!((a.mic_offset_ms, a.system_audio_offset_ms), (-30, 25));
    }

    #[test]
    fn video_quality_options() {
        let a = parse_record(&["a.mkv"]).unwrap();
        assert_eq!(
            (a.b_frames, a.profile.as_deref(), a.preset),
            (0, None, EncoderPreset::Balanced)
        );
        assert_eq!(
            a.keyframe_interval,
            KeyframeInterval::Duration(Duration::from_secs(2))
        );

        let a = parse_record(&[
            "a.mkv",
            "--cbr-padding",
            "--keyframe-interval",
            "4",
            "--b-frames",
            "2",
            "--profile",
            "Main",
            "--preset",
            "quality",
        ])
        .unwrap();
        assert_eq!(
            a.rate_control,
            RateControl::CbrPadded {
                bitrate_bps: 12_000_000
            }
        );
        assert_eq!(
            a.keyframe_interval,
            KeyframeInterval::Duration(Duration::from_secs(4))
        );
        assert_eq!(a.b_frames, 2);
        assert_eq!(a.profile.as_deref(), Some("main"));
        assert_eq!(a.preset, EncoderPreset::Quality);
        assert_eq!(
            describe_video(&a),
            "cbr 12000 kbit/s padded, keyframe every 4 s, 2 B-frames at most, profile main, \
             preset quality"
        );

        let vbr = |args: &[&str]| parse_record(args).unwrap().rate_control;
        assert_eq!(
            vbr(&["a.mkv", "--rate-control", "vbr", "--bitrate", "6000"]),
            RateControl::Vbr {
                target_bps: 6_000_000,
                max_bps: 9_000_000
            }
        );
        assert_eq!(
            vbr(&["a.mkv", "--rate-control", "vbr", "--max-bitrate", "20000"]),
            RateControl::Vbr {
                target_bps: 12_000_000,
                max_bps: 20_000_000
            }
        );
        assert_eq!(
            vbr(&["a.mkv", "--rate-control", "cq"]),
            RateControl::ConstantQuality { level: 23 }
        );
        assert_eq!(
            vbr(&["a.mkv", "--quality", "18", "--rate-control", "cq"]),
            RateControl::ConstantQuality { level: 18 }
        );
    }

    #[test]
    fn bad_arguments_are_refused_with_a_reason() {
        for (args, want) in [
            (&[][..], "output file is missing"),
            (&["a.avi"][..], ".mkv or .mp4"),
            (&["a.mkv", "b.mkv"][..], "unexpected argument"),
            (&["a.mkv", "--size", "1919x1080"][..], "even"),
            (&["a.mkv", "--size", "big"][..], "WxH"),
            (&["a.mkv", "--fps", "0"][..], "above 0"),
            (&["a.mkv", "--seconds"][..], "needs a value"),
            (&["a.mkv", "--loud"][..], "unknown option"),
            (&["a.mkv", "--mic-offset", "201"][..], "more than 200 ms"),
            (
                &["a.mkv", "--system-audio-offset", "-250"][..],
                "more than 200 ms",
            ),
            (&["a.mkv", "--mic-offset", "1.5"][..], "not a number"),
            (
                &["a.mkv", "--rate-control", "abr"][..],
                "not cbr, vbr or cq",
            ),
            (
                &["a.mkv", "--max-bitrate", "9000"][..],
                "needs --rate-control vbr",
            ),
            (&["a.mkv", "--quality", "20"][..], "needs --rate-control cq"),
            (
                &["a.mkv", "--rate-control", "cq", "--quality", "52"][..],
                "above 51",
            ),
            (
                &["a.mkv", "--rate-control", "cq", "--bitrate", "6000"][..],
                "does not apply",
            ),
            (
                &[
                    "a.mkv",
                    "--rate-control",
                    "vbr",
                    "--bitrate",
                    "8000",
                    "--max-bitrate",
                    "6000",
                ][..],
                "at least --bitrate",
            ),
            (
                &["a.mkv", "--rate-control", "vbr", "--cbr-padding"][..],
                "--cbr-padding needs",
            ),
            (&["a.mkv", "--b-frames", "5"][..], "above 4"),
            (&["a.mkv", "--preset", "fast"][..], "not quality"),
            (&["a.mkv", "--keyframe-interval", "0"][..], "above 0"),
        ] {
            let err = parse_record(args).unwrap_err();
            assert!(err.contains(want), "{args:?}: {err}");
        }
        assert!(parse(["play"]).is_err());
        assert_eq!(parse([]).unwrap(), Command::Help);
        assert_eq!(parse(["devices"]).unwrap(), Command::Devices);
    }

    #[test]
    fn token_commands_parse() {
        assert_eq!(
            parse(["token", "list"]).unwrap(),
            Command::Token(TokenCommand::List)
        );
        assert_eq!(
            parse(["token", "create", "Stream Deck"]).unwrap(),
            Command::Token(TokenCommand::Create {
                name: "Stream Deck".into(),
                access: Access::Control
            })
        );
        assert_eq!(
            parse(["token", "create", "--access", "view", "Script"]).unwrap(),
            Command::Token(TokenCommand::Create {
                name: "Script".into(),
                access: Access::View
            })
        );
        assert_eq!(
            parse(["token", "revoke", "Deck"]).unwrap(),
            Command::Token(TokenCommand::Revoke {
                name: "Deck".into()
            })
        );
        for (args, want) in [
            (&["token"][..], "list, create or revoke"),
            (&["token", "show"][..], "unknown command"),
            (&["token", "create"][..], "needs a name"),
            (&["token", "create", "a", "b"][..], "unexpected"),
            (
                &["token", "create", "a", "--access", "admin"][..],
                "not view",
            ),
            (&["token", "create", "a", "--access"][..], "needs a value"),
            (&["token", "create", "a", "--scopes"][..], "unknown option"),
            (&["token", "revoke"][..], "needs the token's name"),
            (&["token", "list", "x"][..], "unexpected"),
        ] {
            let err = parse(args.iter().copied()).unwrap_err();
            assert!(err.contains(want), "{args:?}: {err}");
        }
    }

    /// The URL reaches the file transport as the same path: a file is written there.
    #[test]
    fn the_file_transport_writes_where_the_url_points() {
        use rbs_core::media::{Bytes, ChunkKind, MuxChunk};
        use rbs_transport::{TransportConfig, TransportFactory, TransportInput, TransportItem};
        let dir = std::env::temp_dir().join(format!("rbs-cli url test {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a b%#.mkv");
        let config = TransportConfig::new(
            rbs_transport_file::file_url(&path),
            TransportInput::Container(ContainerId::Matroska),
        );
        let mut t = rbs_transport_file::FileTransportFactory
            .create(&config)
            .unwrap();
        t.start(&[]).unwrap();
        let chunk = MuxChunk::new(ChunkKind::Header, Bytes::from_static(b"rbs"));
        t.send(TransportItem::Chunk(&chunk)).unwrap();
        t.finish(Duration::from_secs(5)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"rbs");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
