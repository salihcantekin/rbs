//! Command-line parsing (no dependency: the grammar is small).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rbs_core::media::{ContainerId, Size};
use rbs_core::timing::FrameRate;

/// What the user asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `rbs-cli record ...`
    Record(RecordArgs),
    /// `rbs-cli devices`: list adapters and capture targets.
    Devices,
    /// `rbs-cli help` or `--help`.
    Help,
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
    /// Video bitrate in bit/s.
    pub video_bitrate: u64,
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
    /// Replace an existing file.
    pub overwrite: bool,
}

/// The usage text.
pub const USAGE: &str = "\
usage:
  rbs-cli record <file.mkv|file.mp4> [options]
  rbs-cli devices

record options:
  --seconds N          stop after N seconds (default: press Enter to stop)
  --size WxH           canvas size (default 1920x1080)
  --fps N              canvas frame rate (default 60)
  --bitrate KBPS       video bitrate in kbit/s (default 12000)
  --audio-bitrate KBPS audio bitrate in kbit/s (default 160)
  --encoder ID         use only this video encoder: an adapter id or its last part (nvenc, amf)
  --display N          display number from `devices` (default 0)
  --camera N           camera number from `devices` (default 0)
  --no-camera          no camera picture
  --no-mic             no microphone
  --no-system-audio    no system audio
  --no-audio           no audio track at all
  --overwrite          replace the file if it exists";

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
        Some("record") => record(it).map(Command::Record),
        Some(other) => Err(format!("unknown command {other:?}")),
    }
}

fn record<'a>(mut it: impl Iterator<Item = &'a str>) -> Result<RecordArgs, String> {
    let mut out = None;
    let mut a = RecordArgs {
        out: PathBuf::new(),
        container: ContainerId::Matroska,
        duration: None,
        size: Size::new(1920, 1080),
        fps: FrameRate::FPS_60,
        video_bitrate: 12_000_000,
        audio_bitrate: 160_000,
        encoder: None,
        display: None,
        camera: None,
        no_camera: false,
        no_mic: false,
        no_system_audio: false,
        no_audio: false,
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
            "--bitrate" => {
                a.video_bitrate = u64::from(positive(value(&mut it, arg)?, arg)?) * 1000;
            }
            "--audio-bitrate" => {
                a.audio_bitrate = u64::from(positive(value(&mut it, arg)?, arg)?) * 1000;
            }
            "--encoder" => a.encoder = Some(value(&mut it, arg)?.to_owned()),
            "--display" => a.display = Some(number(value(&mut it, arg)?, arg)?),
            "--camera" => a.camera = Some(number(value(&mut it, arg)?, arg)?),
            "--no-camera" => a.no_camera = true,
            "--no-mic" => a.no_mic = true,
            "--no-system-audio" => a.no_system_audio = true,
            "--no-audio" => a.no_audio = true,
            "--overwrite" => a.overwrite = true,
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            path if out.is_none() => out = Some(PathBuf::from(path)),
            extra => return Err(format!("unexpected argument {extra:?}")),
        }
    }
    a.out = out.ok_or("record: the output file is missing")?;
    a.container = container_of(&a.out)?;
    Ok(a)
}

/// Escapes what a URL path must not carry raw: `%`, space, `#`, `?` and `\` (a name
/// character on Unix). The file transport decodes them.
fn escape(part: &str, url: &mut String) {
    for c in part.chars() {
        match c {
            '%' => url.push_str("%25"),
            ' ' => url.push_str("%20"),
            '#' => url.push_str("%23"),
            '?' => url.push_str("%3F"),
            '\\' => url.push_str("%5C"),
            c => url.push(c),
        }
    }
}

/// The `file://` URL of an absolute path (RFC 8089), built from the path's components:
/// `C:\x\a b.mkv` and `\\?\C:\x\a b.mkv` become `file:///C:/x/a%20b.mkv`,
/// `\\srv\share\a.mkv` becomes `file://srv/share/a.mkv`, `/tmp/a.mkv` becomes
/// `file:///tmp/a.mkv`.
pub fn file_url(path: &Path) -> String {
    use std::path::{Component, Prefix};
    let mut host = String::new();
    let mut parts: Vec<String> = Vec::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => match p.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                    parts.push(format!("{}:", char::from(d)));
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    host = server.to_string_lossy().into_owned();
                    parts.push(share.to_string_lossy().into_owned());
                }
                _ => parts.push(p.as_os_str().to_string_lossy().into_owned()),
            },
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => parts.push("..".to_owned()),
            Component::Normal(n) => parts.push(n.to_string_lossy().into_owned()),
        }
    }
    let mut url = format!("file://{host}");
    for part in &parts {
        url.push('/');
        escape(part, &mut url);
    }
    url
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn parse_record(args: &[&str]) -> Result<RecordArgs, String> {
        match parse(std::iter::once("record").chain(args.iter().copied()))? {
            Command::Record(r) => Ok(r),
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
        ])
        .unwrap();
        assert_eq!(a.container, ContainerId::Mp4Fragmented);
        assert_eq!(a.duration, Some(Duration::from_secs(30)));
        assert_eq!(a.size, Size::new(1280, 720));
        assert_eq!(a.fps, FrameRate::FPS_30);
        assert_eq!(a.video_bitrate, 8_000_000);
        assert_eq!(a.audio_bitrate, 192_000);
        assert_eq!(a.encoder.as_deref(), Some("amf"));
        assert_eq!(a.display, Some(1));
        assert!(a.no_camera && a.no_system_audio && !a.no_mic);
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
        ] {
            let err = parse_record(args).unwrap_err();
            assert!(err.contains(want), "{args:?}: {err}");
        }
        assert!(parse(["play"]).is_err());
        assert_eq!(parse([]).unwrap(), Command::Help);
        assert_eq!(parse(["devices"]).unwrap(), Command::Devices);
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_become_file_urls() {
        for (path, url) in [
            (
                r"C:\Users\me\My Videos\a%1.mkv",
                "file:///C:/Users/me/My%20Videos/a%251.mkv",
            ),
            (r"\\?\C:\x\a.mkv", "file:///C:/x/a.mkv"),
            (r"\\srv\share\rec\a.mkv", "file://srv/share/rec/a.mkv"),
        ] {
            assert_eq!(file_url(Path::new(path)), url, "{path}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_paths_become_file_urls() {
        assert_eq!(file_url(Path::new("/tmp/a.mkv")), "file:///tmp/a.mkv");
        assert_eq!(
            file_url(Path::new("/tmp/a b\\c#?.mkv")),
            "file:///tmp/a%20b%5Cc%23%3F.mkv"
        );
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
            file_url(&path),
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
