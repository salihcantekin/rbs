//! Command-line parsing (no dependency: the grammar is small).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rbs_codec::{EncoderPreset, KeyframeInterval, RateControl};
use rbs_core::media::{ContainerId, Size};
use rbs_core::timing::FrameRate;
use rbs_remote_settings::Access;
use rbs_transport::TransportInput;

/// What the user asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `rbs-cli record ...`, or `rbs-cli stream ...` (with [`RecordArgs::stream`] set).
    Record(Box<RecordArgs>),
    /// `rbs-cli devices`: list adapters and capture targets.
    Devices,
    /// `rbs-cli token ...`: manage remote control access tokens.
    Token(TokenCommand),
    /// `rbs-cli app ...`: control a running RBS desktop app over local IPC.
    App(AppCommand),
    /// `rbs-cli remote fingerprint`: print the key pin and certificate fingerprint
    /// clients on other computers check (ADR-0046).
    RemoteFingerprint,
    /// `rbs-cli vcam ...`: the RBS virtual camera (ADR-0066, Windows).
    Vcam(VcamCommand),
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

/// The virtual camera's announced format, one of ADR-0066 decision 11.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VcamFormat {
    /// Size in pixels.
    pub size: Size,
    /// Frames per second.
    pub fps: u32,
}

impl Default for VcamFormat {
    fn default() -> Self {
        Self {
            size: Size::new(1280, 720),
            fps: 30,
        }
    }
}

/// Sizes the virtual camera can announce (ADR-0066 decision 11).
pub const VCAM_SIZES: [(u32, u32); 3] = [(1920, 1080), (1280, 720), (640, 360)];
/// Frame rates the virtual camera can announce (ADR-0066 decision 11).
pub const VCAM_RATES: [u32; 4] = [15, 24, 30, 60];

/// `rbs-cli vcam ...`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcamCommand {
    /// `vcam status`: whether the camera is added, in words.
    Status,
    /// `vcam add [--size WxH] [--fps N]`: adds the camera (one administrator prompt the
    /// first time) and sets its format.
    Add(VcamFormat),
    /// `vcam format [--size WxH] [--fps N]`: changes the format it announces.
    Format(VcamFormat),
    /// `vcam remove`: removes the camera (the class registration stays).
    Remove,
}

/// `rbs-cli app ...`: what to ask a running RBS desktop app (control API over local
/// IPC).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppCommand {
    /// `app stream ...`.
    Stream(StreamControl),
}

/// `rbs-cli app stream ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamControl {
    /// `start`: go live with the app's saved stream settings.
    Start,
    /// `stop`: end its stream.
    Stop,
    /// `status [--json]`: its stream state.
    Status {
        /// Print the state as the control API's JSON.
        json: bool,
    },
}

/// Where `stream` sends the program: the URL and its transport settings. The stream key
/// is read from an environment variable when the stream starts, never from the command
/// line (which other users and the shell history can see).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamTarget {
    /// `rtmp://host[:port]/app` or `srt://host:port`.
    pub url: String,
    /// Packets for RTMP, MPEG-TS chunks for SRT.
    pub input: TransportInput,
    /// The environment variable holding the stream key (RTMP) or stream id (SRT).
    pub key_env: String,
    /// SRT latency in ms (`latency_ms` transport option); `None`: the transport's default.
    pub srt_latency_ms: Option<u16>,
    /// Reconnect when the connection drops.
    pub reconnect: bool,
}

/// Environment variable read for the stream key unless `--key-env` names another.
pub const DEFAULT_KEY_ENV: &str = "RBS_STREAM_KEY";

/// Default video bitrate of `stream` in kbit/s: what streaming services take for
/// 1080p60 H.264 (`record` defaults to 12000).
pub const DEFAULT_STREAM_KBPS: u64 = 6000;

/// The transport input a stream URL needs: packets for `rtmp://`, MPEG-TS for `srt://`.
pub fn stream_input(url: &str) -> Result<TransportInput, String> {
    let scheme = url
        .split_once("://")
        .map(|(s, _)| s.to_ascii_lowercase())
        .unwrap_or_default();
    match scheme.as_str() {
        "rtmp" => Ok(TransportInput::Packets),
        "srt" => Ok(TransportInput::Container(ContainerId::MpegTs)),
        _ => Err(format!(
            "{url}: a stream URL starts with rtmp:// or srt:// (rtmps is not supported yet)"
        )),
    }
}

/// Options of `record` and `stream`.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordArgs {
    /// The file written (`record`); empty for `stream`.
    pub out: PathBuf,
    /// `stream`: where the program goes instead of a file.
    pub stream: Option<StreamTarget>,
    /// The container, from the file's extension (`record`), or MPEG-TS for SRT.
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
    /// `--vcam`: also send the program to the virtual camera, in this format.
    pub vcam: Option<VcamFormat>,
}

/// The usage text.
pub const USAGE: &str = "\
usage:
  rbs-cli record <file.mkv|file.mp4> [options]
  rbs-cli stream <rtmp://host/app|srt://host:port> [stream options] [options]
  rbs-cli devices
  rbs-cli token list
  rbs-cli token create <name> [--access view|control|full]
  rbs-cli token revoke <name>
  rbs-cli remote fingerprint
  rbs-cli app stream start|stop|status [--json]
  rbs-cli vcam status|add|format|remove [--size WxH] [--fps N]

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
  --overwrite          replace the file if it exists (record only)
  --vcam               also send the program to the RBS virtual camera (Windows;
                       add it first with `vcam add`)
  --vcam-size WxH      the camera's size: 1920x1080, 1280x720 or 640x360
                       (default 1280x720)
  --vcam-fps N         the camera's frame rate: 15, 24, 30 or 60 (default 30)

stream options (the record options above apply too, except --overwrite; the
default video bitrate is 6000 kbit/s):
  --key-env NAME       read the stream key (RTMP) or stream id (SRT) from the
                       environment variable NAME (default RBS_STREAM_KEY); it is
                       never taken on the command line
  --srt-latency MS     SRT latency in ms, 0 to 65535 (default 120)
  --no-reconnect       stop when the connection drops (default: reconnect, up
                       to 10 tries, 1 s to 30 s apart)

token commands (access tokens for apps that control RBS, as in Settings, Remote
control; the token is printed once, keep it):
  --access LEVEL       view: see state only; control: also change sources and
                       start and stop recording (default, right for panels);
                       full: also change settings

app commands (control the running RBS app; it needs Allow remote control on in
its Settings, Remote control):
  stream start         go live with the server, key and encoder settings saved in
                       the app's Settings, Stream
  stream stop          end the stream (the app does not ask first)
  stream status        the stream's state and connection; --json prints it as
                       the control API's JSON

vcam commands (the RBS virtual camera that meeting apps can pick; Windows 11):
  status               whether the camera is added
  add                  add the camera; Windows asks for administrator approval
                       the first time only. --size and --fps as for format
  format               change what the camera announces: --size 1920x1080,
                       1280x720 or 640x360 (default 1280x720), --fps 15, 24, 30
                       or 60 (default 30); applies when no app uses the camera
  remove               remove the camera";

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
    default_bps: u64,
    mode: Option<String>,
    bitrate_bps: Option<u64>,
    max_bps: Option<u64>,
    quality: Option<u8>,
    padded: bool,
}

impl RateFlags {
    fn rate_control(self) -> Result<RateControl, String> {
        let bitrate = self.bitrate_bps.unwrap_or(self.default_bps);
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
    let args: Vec<&str> = args.into_iter().collect();
    // `--help` anywhere (`rbs-cli stream --help`) shows the usage, as `help` does.
    if args.iter().any(|a| matches!(*a, "--help" | "-h")) {
        return Ok(Command::Help);
    }
    let mut it = args.into_iter();
    match it.next() {
        None | Some("help" | "--help" | "-h") => Ok(Command::Help),
        Some("devices") => match it.next() {
            None => Ok(Command::Devices),
            Some(extra) => Err(format!("devices: unexpected {extra:?}")),
        },
        Some("record") => record(it, false).map(|a| Command::Record(Box::new(a))),
        Some("stream") => record(it, true).map(|a| Command::Record(Box::new(a))),
        Some("token") => token(it).map(Command::Token),
        Some("app") => app(it).map(Command::App),
        Some("vcam") => vcam(it).map(Command::Vcam),
        Some("remote") => match (it.next(), it.next()) {
            (Some("fingerprint"), None) => Ok(Command::RemoteFingerprint),
            (Some("fingerprint"), Some(extra)) => {
                Err(format!("remote fingerprint: unexpected {extra:?}"))
            }
            (Some(other), _) => Err(format!("remote: unknown command {other:?}")),
            (None, _) => Err("remote needs fingerprint".into()),
        },
        Some(other) => Err(format!("unknown command {other:?}")),
    }
}

fn vcam_size(text: &str, flag: &str) -> Result<Size, String> {
    let s = size(text).map_err(|e| e.replacen("--size", flag, 1))?;
    if VCAM_SIZES.contains(&(s.width, s.height)) {
        Ok(s)
    } else {
        Err(format!(
            "{flag}: {text} is not 1920x1080, 1280x720 or 640x360"
        ))
    }
}

fn vcam_fps(text: &str, flag: &str) -> Result<u32, String> {
    let n = positive(text, flag)?;
    if VCAM_RATES.contains(&n) {
        Ok(n)
    } else {
        Err(format!("{flag}: {n} is not 15, 24, 30 or 60"))
    }
}

fn vcam<'a>(mut it: impl Iterator<Item = &'a str>) -> Result<VcamCommand, String> {
    let verb = it
        .next()
        .ok_or("vcam needs status, add, format or remove")?;
    let takes_format = matches!(verb, "add" | "format");
    let mut format = VcamFormat::default();
    while let Some(arg) = it.next() {
        match arg {
            "--size" if takes_format => format.size = vcam_size(value(&mut it, arg)?, arg)?,
            "--fps" if takes_format => format.fps = vcam_fps(value(&mut it, arg)?, arg)?,
            extra => return Err(format!("vcam {verb}: unexpected {extra:?}")),
        }
    }
    match verb {
        "status" => Ok(VcamCommand::Status),
        "add" => Ok(VcamCommand::Add(format)),
        "format" => Ok(VcamCommand::Format(format)),
        "remove" => Ok(VcamCommand::Remove),
        other => Err(format!("vcam: unknown command {other:?}")),
    }
}

fn app<'a>(mut it: impl Iterator<Item = &'a str>) -> Result<AppCommand, String> {
    match it.next() {
        Some("stream") => {}
        Some(other) => return Err(format!("app: unknown command {other:?}")),
        None => return Err("app needs stream start, stream stop or stream status".into()),
    }
    let command = match it.next() {
        Some("start") => StreamControl::Start,
        Some("stop") => StreamControl::Stop,
        Some("status") => {
            let json = match it.next() {
                None => false,
                Some("--json") => true,
                Some(extra) => return Err(format!("app stream status: unexpected {extra:?}")),
            };
            StreamControl::Status { json }
        }
        Some(other) => return Err(format!("app stream: unknown command {other:?}")),
        None => return Err("app stream needs start, stop or status".into()),
    };
    match it.next() {
        None => Ok(AppCommand::Stream(command)),
        Some(extra) => Err(format!("app stream: unexpected {extra:?}")),
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

fn record<'a>(mut it: impl Iterator<Item = &'a str>, stream: bool) -> Result<RecordArgs, String> {
    let mut out = None;
    let mut rate = RateFlags {
        default_bps: if stream {
            DEFAULT_STREAM_KBPS * 1000
        } else {
            12_000_000
        },
        ..RateFlags::default()
    };
    let mut key_env = None;
    let mut srt_latency_ms = None;
    let mut reconnect = true;
    let mut a = RecordArgs {
        out: PathBuf::new(),
        stream: None,
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
        vcam: None,
    };
    let mut vcam = false;
    let mut vcam_format = VcamFormat::default();
    let mut vcam_options = false;
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
            "--overwrite" if !stream => a.overwrite = true,
            "--vcam" => vcam = true,
            "--vcam-size" => {
                vcam_format.size = vcam_size(value(&mut it, arg)?, arg)?;
                vcam_options = true;
            }
            "--vcam-fps" => {
                vcam_format.fps = vcam_fps(value(&mut it, arg)?, arg)?;
                vcam_options = true;
            }
            "--key-env" if stream => {
                let name = value(&mut it, arg)?;
                if name.is_empty() || name.contains('=') {
                    return Err(format!("{arg}: {name:?} is not a variable name"));
                }
                key_env = Some(name.to_owned());
            }
            "--srt-latency" if stream => {
                srt_latency_ms = Some(number::<u16>(value(&mut it, arg)?, arg)?);
            }
            "--no-reconnect" if stream => reconnect = false,
            flag if flag.starts_with("--") => {
                let command = if stream { "stream" } else { "record" };
                return Err(format!("{command}: unknown option {flag}"));
            }
            path if out.is_none() => out = Some(path),
            extra => return Err(format!("unexpected argument {extra:?}")),
        }
    }
    a.rate_control = rate.rate_control()?;
    if vcam_options && !vcam {
        return Err("--vcam-size and --vcam-fps need --vcam".into());
    }
    a.vcam = vcam.then_some(vcam_format);
    if stream {
        let url = out.ok_or("stream: the URL is missing (rtmp://host/app or srt://host:port)")?;
        let input = stream_input(url)?;
        if srt_latency_ms.is_some() && input == TransportInput::Packets {
            return Err("--srt-latency applies to srt:// URLs only".into());
        }
        if let TransportInput::Container(c) = input {
            a.container = c;
        }
        a.stream = Some(StreamTarget {
            url: url.to_owned(),
            input,
            key_env: key_env.unwrap_or_else(|| DEFAULT_KEY_ENV.to_owned()),
            srt_latency_ms,
            reconnect,
        });
    } else {
        a.out = PathBuf::from(out.ok_or("record: the output file is missing")?);
        a.container = container_of(&a.out)?;
    }
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
    fn help_after_a_command_shows_the_usage() {
        for args in [
            &["stream", "--help"][..],
            &["record", "-h"][..],
            &["stream", "rtmp://h/app", "--bitrate", "6000", "--help"][..],
            &["token", "--help"][..],
        ] {
            assert_eq!(
                parse(args.iter().copied()).unwrap(),
                Command::Help,
                "{args:?}"
            );
        }
    }

    fn parse_stream(args: &[&str]) -> Result<RecordArgs, String> {
        match parse(std::iter::once("stream").chain(args.iter().copied()))? {
            Command::Record(r) => Ok(*r),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stream_defaults_and_options() {
        let a = parse_stream(&["rtmp://live.example.com/app"]).unwrap();
        assert_eq!(a.out, PathBuf::new());
        assert_eq!(
            a.stream,
            Some(StreamTarget {
                url: "rtmp://live.example.com/app".into(),
                input: TransportInput::Packets,
                key_env: DEFAULT_KEY_ENV.into(),
                srt_latency_ms: None,
                reconnect: true,
            })
        );
        assert_eq!(
            a.rate_control,
            RateControl::Cbr {
                bitrate_bps: 6_000_000
            }
        );
        assert_eq!(
            a.keyframe_interval,
            KeyframeInterval::Duration(Duration::from_secs(2))
        );

        let a = parse_stream(&[
            "SRT://10.0.0.2:9000",
            "--key-env",
            "MY_ID",
            "--srt-latency",
            "400",
            "--no-reconnect",
            "--bitrate",
            "4500",
            "--seconds",
            "10",
            "--no-camera",
        ])
        .unwrap();
        let s = a.stream.clone().unwrap();
        assert_eq!(s.input, TransportInput::Container(ContainerId::MpegTs));
        assert_eq!(a.container, ContainerId::MpegTs);
        assert_eq!(
            (s.key_env.as_str(), s.srt_latency_ms, s.reconnect),
            ("MY_ID", Some(400), false)
        );
        assert_eq!(
            a.rate_control,
            RateControl::Cbr {
                bitrate_bps: 4_500_000
            }
        );
        assert_eq!(a.duration, Some(Duration::from_secs(10)));
        assert!(a.no_camera);
        // `record` keeps its own default bitrate.
        assert_eq!(
            parse_record(&["a.mkv"]).unwrap().rate_control,
            RateControl::Cbr {
                bitrate_bps: 12_000_000
            }
        );
    }

    #[test]
    fn bad_stream_arguments_are_refused_with_a_reason() {
        for (args, want) in [
            (&[][..], "URL is missing"),
            (&["a.mkv"][..], "rtmp:// or srt://"),
            (&["rtmps://x/app"][..], "rtmp:// or srt://"),
            (&["rtmp://x/app", "--overwrite"][..], "unknown option"),
            (
                &["rtmp://x/app", "--srt-latency", "100"][..],
                "srt:// URLs only",
            ),
            (&["srt://x:1", "--srt-latency", "70000"][..], "not a number"),
            (&["rtmp://x/app", "--key-env", "A=B"][..], "not a variable"),
            (&["rtmp://x/app", "--key"][..], "unknown option"),
            (&["rtmp://x/app", "rtmp://y/app"][..], "unexpected argument"),
        ] {
            let err = parse_stream(args).unwrap_err();
            assert!(err.contains(want), "{args:?}: {err}");
        }
        // Stream options are not record options.
        let err = parse_record(&["a.mkv", "--no-reconnect"]).unwrap_err();
        assert!(err.contains("unknown option"), "{err}");
    }

    #[test]
    fn app_commands_parse() {
        let p = |a: &[&str]| parse(a.iter().copied());
        assert_eq!(
            p(&["app", "stream", "start"]),
            Ok(Command::App(AppCommand::Stream(StreamControl::Start)))
        );
        assert_eq!(
            p(&["app", "stream", "stop"]),
            Ok(Command::App(AppCommand::Stream(StreamControl::Stop)))
        );
        assert_eq!(
            p(&["app", "stream", "status"]),
            Ok(Command::App(AppCommand::Stream(StreamControl::Status {
                json: false
            })))
        );
        assert_eq!(
            p(&["app", "stream", "status", "--json"]),
            Ok(Command::App(AppCommand::Stream(StreamControl::Status {
                json: true
            })))
        );
        for bad in [
            &["app"][..],
            &["app", "record"],
            &["app", "stream"],
            &["app", "stream", "go"],
            &["app", "stream", "start", "rtmp://x/app"],
            &["app", "stream", "status", "--yaml"],
        ] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn token_commands_parse() {
        assert_eq!(
            parse(["remote", "fingerprint"]).unwrap(),
            Command::RemoteFingerprint
        );
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
            (&["remote"][..], "needs fingerprint"),
            (&["remote", "pin"][..], "unknown command"),
            (&["remote", "fingerprint", "x"][..], "unexpected"),
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

    #[test]
    fn vcam_commands() {
        let p = |a: &[&str]| parse(a.iter().copied());
        assert_eq!(
            p(&["vcam", "status"]),
            Ok(Command::Vcam(VcamCommand::Status))
        );
        assert_eq!(
            p(&["vcam", "remove"]),
            Ok(Command::Vcam(VcamCommand::Remove))
        );
        assert_eq!(
            p(&["vcam", "add"]),
            Ok(Command::Vcam(VcamCommand::Add(VcamFormat::default())))
        );
        assert_eq!(
            p(&["vcam", "format", "--size", "1920x1080", "--fps", "60"]),
            Ok(Command::Vcam(VcamCommand::Format(VcamFormat {
                size: Size::new(1920, 1080),
                fps: 60
            })))
        );
        for bad in [
            &["vcam"][..],
            &["vcam", "start"],
            &["vcam", "status", "--fps", "30"],
            &["vcam", "remove", "--size", "1280x720"],
            &["vcam", "format", "--size", "1000x500"],
            &["vcam", "format", "--fps", "25"],
            &["vcam", "add", "--size"],
        ] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn record_with_the_virtual_camera() {
        assert_eq!(parse_record(&["a.mkv"]).unwrap().vcam, None);
        assert_eq!(
            parse_record(&["a.mkv", "--vcam"]).unwrap().vcam,
            Some(VcamFormat::default())
        );
        assert_eq!(
            parse_stream(&[
                "rtmp://h/app",
                "--vcam",
                "--vcam-size",
                "640x360",
                "--vcam-fps",
                "15"
            ])
            .unwrap()
            .vcam,
            Some(VcamFormat {
                size: Size::new(640, 360),
                fps: 15
            })
        );
        assert!(parse_record(&["a.mkv", "--vcam-fps", "30"]).is_err());
        assert!(parse_record(&["a.mkv", "--vcam", "--vcam-size", "1920x1200"]).is_err());
    }
}
