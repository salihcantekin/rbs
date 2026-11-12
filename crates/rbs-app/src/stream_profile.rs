//! Where Go live sends the program and how it is encoded: the server URL, whether a
//! stream key is saved (the key itself lives only in the system's credential store,
//! `stream_key`), reconnecting, the SRT latency and the encoder settings. Kept in the
//! `stream` section of the output profile in use (ADR-0051); `stream.toml`, which
//! older versions kept, is read once to make the first profile. Toolkit-free and tested without a window.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rbs_core::media::ContainerId;
use rbs_transport::{ReconnectPolicy, TransportInput};
use serde::{Deserialize, Serialize};

use crate::record_profile::{Encoding, OutputSize, RateMode, output_size_problem};

/// The file format version this build writes and reads.
const FORMAT: u32 = 1;

/// Default video bitrate of a stream in kbit/s: what streaming services take for
/// 1080p H.264 (as `rbs-cli stream`).
pub const DEFAULT_STREAM_KBPS: u32 = 6_000;

/// SRT latencies offered besides the transport's default (120 ms), in ms.
pub const SRT_LATENCIES_MS: [u16; 4] = [200, 500, 1_000, 2_000];

/// The SRT transport's own latency when none is set, in ms.
pub const SRT_DEFAULT_LATENCY_MS: u16 = 120;

/// Reconnect policy when reconnecting is on: up to 10 tries, 1 s doubling to 30 s apart
/// (as `rbs-cli stream`).
pub const RECONNECT: ReconnectPolicy = ReconnectPolicy::Backoff {
    initial: Duration::from_secs(1),
    max: Duration::from_secs(30),
    attempts: Some(10),
};

/// How the program reaches the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// `rtmp://host[:port]/app`: H.264 and AAC packets, the stream key required.
    Rtmp,
    /// `srt://host:port`: MPEG-TS, the stream id optional.
    Srt,
}

impl Protocol {
    /// The name in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Protocol::Rtmp => "RTMP",
            Protocol::Srt => "SRT",
        }
    }

    /// What the transport takes: packets for RTMP, MPEG-TS chunks for SRT.
    pub fn input(self) -> TransportInput {
        match self {
            Protocol::Rtmp => TransportInput::Packets,
            Protocol::Srt => TransportInput::Container(ContainerId::MpegTs),
        }
    }

    /// The server needs a stream key (RTMP); SRT's stream id is optional.
    pub fn needs_key(self) -> bool {
        self == Protocol::Rtmp
    }
}

/// A server URL taken apart: its protocol and its host name (for the live indicator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    /// The protocol.
    pub protocol: Protocol,
    /// The host name or address, without port or path.
    pub host: String,
}

/// Checks a server URL: `rtmp://host[:port]/app` or `srt://host:port`.
pub fn parse_server(url: &str) -> Result<Server, String> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(format!(
            "{url} is not a server address: it starts with rtmp:// or srt://."
        ));
    };
    let protocol = match scheme.to_ascii_lowercase().as_str() {
        "rtmp" => Protocol::Rtmp,
        "srt" => Protocol::Srt,
        other => {
            return Err(format!(
                "RBS streams over rtmp:// and srt:// so far; {other}:// is not supported yet."
            ));
        }
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, p),
        None => (rest, ""),
    };
    if rest.contains('?') {
        // `?streamid=` or `?passphrase=` would put a secret in the URL, the settings
        // file and the log: secrets go in the Stream key field only.
        return Err(format!(
            "{}: an address cannot have options after ?. Put the stream key or stream id in the Stream key field.",
            url.split_once('?').map_or(url, |(u, _)| u)
        ));
    }
    let (host, port) = match authority.strip_prefix('[') {
        // An IPv6 address: `[::1]:port`.
        Some(v6) => match v6.split_once(']') {
            Some((h, after)) => (h, after.strip_prefix(':')),
            None => return Err(format!("{url}: the address in [ ] is not closed.")),
        },
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        },
    };
    if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c == '@') {
        return Err(format!("{url} has no server name."));
    }
    if let Some(p) = port
        && p.parse::<u16>().map_or(true, |p| p == 0)
    {
        return Err(format!("{url}: {p} is not a port number."));
    }
    match protocol {
        Protocol::Rtmp if path.trim_matches('/').is_empty() => Err(format!(
            "{url} has no application name: it looks like rtmp://server/app."
        )),
        Protocol::Srt if port.is_none() => Err(format!(
            "{url} has no port: it looks like srt://server:port."
        )),
        _ => Ok(Server {
            protocol,
            host: host.to_owned(),
        }),
    }
}

fn yes() -> bool {
    true
}

fn stream_encoding() -> Encoding {
    Encoding::new(DEFAULT_STREAM_KBPS)
}

/// What the next stream uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamProfile {
    /// File format version (1).
    pub format_version: u32,
    /// The server URL (`rtmp://host/app` or `srt://host:port`); empty until set. Never
    /// holds the key.
    #[serde(default)]
    pub server: String,
    /// A stream key was saved in the credential store (the key itself is never here).
    #[serde(default)]
    pub key_saved: bool,
    /// Reconnect when the connection drops ([`RECONNECT`]).
    #[serde(default = "yes")]
    pub reconnect: bool,
    /// SRT latency in ms; `None`: the transport's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srt_latency_ms: Option<u16>,
    /// The encoder settings.
    #[serde(default = "stream_encoding")]
    pub encoding: Encoding,
    /// Record when the stream starts, with the recording settings; ending the stream
    /// stops that recording. Off by default.
    #[serde(default)]
    pub auto_record: bool,
    /// Encoded picture size. Omitted until the user picks one: absent means this
    /// stream's canvas (ADR-0102).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_size: Option<OutputSize>,
}

impl StreamProfile {
    /// No server yet, no key, reconnecting on, the SRT default latency, and CBR
    /// 6000 kbit/s video with the recording's other defaults.
    pub fn new() -> Self {
        Self {
            format_version: FORMAT,
            server: String::new(),
            key_saved: false,
            reconnect: true,
            srt_latency_ms: None,
            encoding: stream_encoding(),
            auto_record: false,
            output_size: None,
        }
    }

    /// The server taken apart, `None` when it is empty or not valid.
    pub fn parsed(&self) -> Option<Server> {
        parse_server(&self.server).ok()
    }

    /// The reconnect policy.
    pub fn reconnect_policy(&self) -> ReconnectPolicy {
        if self.reconnect {
            RECONNECT
        } else {
            ReconnectPolicy::Off
        }
    }

    /// Opaque transport options: the SRT latency when one is set and the server is SRT.
    pub fn transport_options(&self) -> Vec<(String, String)> {
        match (self.parsed().map(|s| s.protocol), self.srt_latency_ms) {
            (Some(Protocol::Srt), Some(ms)) => vec![("latency_ms".to_owned(), ms.to_string())],
            _ => Vec::new(),
        }
    }

    /// Why this profile cannot be used to go live, in words, or `None`. An empty server
    /// is not a problem of the file: [`StreamProfile::missing`] says it.
    pub fn problem(&self) -> Option<String> {
        if self.format_version != FORMAT {
            return Some(format!(
                "settings version {} is not supported",
                self.format_version
            ));
        }
        if !self.server.is_empty()
            && let Err(why) = parse_server(&self.server)
        {
            return Some(why);
        }
        if let Some(ms) = self.srt_latency_ms
            && ms == 0
        {
            return Some("An SRT latency of 0 ms is not possible.".into());
        }
        self.encoding
            .problem()
            .or_else(|| output_size_problem(self.output_size))
    }

    /// What is still missing before Go live can work: the server, or (RTMP) the key.
    pub fn missing(&self) -> Option<&'static str> {
        if self.server.is_empty() {
            return Some("Enter the server in Settings, Stream.");
        }
        match self.parsed() {
            Some(s) if s.protocol.needs_key() && !self.key_saved => {
                Some("Enter the stream key in Settings, Stream.")
            }
            _ => None,
        }
    }
}

impl StreamProfile {
    /// These settings as the control API shows them (`settings.get`): the server's
    /// protocol but not its address, and whether a key is saved.
    pub fn to_api(&self) -> rbs_api::types::StreamSettings {
        use rbs_api::types as api;
        api::StreamSettings {
            protocol: self.parsed().map(|s| match s.protocol {
                Protocol::Rtmp => api::StreamProtocol::Rtmp,
                Protocol::Srt => api::StreamProtocol::Srt,
            }),
            key_saved: self.key_saved,
            reconnect: self.reconnect,
            srt_latency_ms: self.srt_latency_ms,
            encoding: self.encoding.to_api(),
            auto_record: self.auto_record,
            output_size: self.output_size.map(|s| rbs_api::types::Dimensions {
                width: s.width,
                height: s.height,
            }),
        }
    }

    /// These settings with what a control API client may change (`settings.set`)
    /// replaced: the server and the key stay. Refused, in words, when the result cannot
    /// be used.
    pub fn with_api(&self, patch: &rbs_api::types::StreamSettingsPatch) -> Result<Self, String> {
        let mut p = self.clone();
        p.reconnect = patch.reconnect;
        p.srt_latency_ms = patch.srt_latency_ms;
        p.encoding = Encoding::from_api(&patch.encoding);
        if let Some(on) = patch.auto_record {
            p.auto_record = on;
        }
        // Omitted or null stores the same as this stream's canvas (protocol 1.39).
        p.output_size = patch.output_size.map(|s| OutputSize {
            width: s.width,
            height: s.height,
        });
        // Free text the form never offers would make every later Go live fail.
        if let Some(name) = &p.encoding.profile
            && !["high", "main", "baseline"].contains(&name.as_str())
        {
            return Err(format!(
                "codec profile {name:?} is not high, main or baseline"
            ));
        }
        if p.encoding
            .encoder
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 128 || id.chars().any(char::is_control))
        {
            return Err("the encoder id is not an adapter id".into());
        }
        match p.problem() {
            // The saved server is not the caller's to read: a problem with it is not
            // repeated.
            Some(_) if !p.server.is_empty() && parse_server(&p.server).is_err() => {
                Err("the saved server cannot be used: see Settings, Stream".into())
            }
            Some(why) => Err(why),
            None => Ok(p),
        }
    }
}

/// What "Record when the stream starts" costs, for the notice under the setting.
/// `same_encoder`: both pick the same encoder (both automatic, or the same one). With
/// the same video settings as well the recording joins the stream's video encoder
/// (ADR-0047 decision 3), otherwise every frame is encoded twice. Either way the file
/// grows for as long as the stream runs: about so many GB an hour at the recording's
/// bitrate (VBR peaks aside; not known for constant quality). Each variant is one whole
/// text, so it can be translated as one (U86).
pub fn auto_record_note(record: &Encoding, stream: &Encoding, same_encoder: bool) -> String {
    let shared = same_encoder && record.video_settings() == stream.video_settings();
    let gb = match record.rate_control {
        RateMode::ConstantQuality => None,
        _ => {
            // kbit/s for an hour is kbit/s * 3600 / 8 kB; GB with one decimal.
            let kbps = u64::from(record.video_kbps) + u64::from(record.audio_kbps);
            let tenths = (kbps * 3600 / 8 + 50_000) / 100_000;
            Some(format!("{}.{}", tenths / 10, tenths % 10))
        }
    };
    match (shared, gb) {
        (true, Some(gb)) => format!(
            "The recording and the stream have the same video settings, so they share one \
             encoder and the extra processor and graphics load is small. The recording needs \
             about {gb} GB of disk space for each hour of streaming."
        ),
        (true, None) => "The recording and the stream have the same video settings, so they \
             share one encoder and the extra processor and graphics load is small. The \
             recording needs free disk space for as long as the stream runs."
            .to_owned(),
        (false, Some(gb)) => format!(
            "The recording and the stream have different video settings, so every frame is \
             encoded twice: expect more processor or graphics load while live. Use the same \
             settings in Recording and Stream to share one encoder. The recording needs about \
             {gb} GB of disk space for each hour of streaming."
        ),
        (false, None) => "The recording and the stream have different video settings, so \
             every frame is encoded twice: expect more processor or graphics load while live. \
             Use the same settings in Recording and Stream to share one encoder. The recording \
             needs free disk space for as long as the stream runs."
            .to_owned(),
    }
}

impl Default for StreamProfile {
    fn default() -> Self {
        Self::new()
    }
}

fn file(config: &Path) -> PathBuf {
    config.join("stream.toml")
}

/// The saved profile; the defaults when there is none. A file that cannot be read gives
/// the defaults and the reason.
pub fn load(config: &Path) -> (StreamProfile, Option<String>) {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (StreamProfile::new(), None),
        Err(e) => {
            return (
                StreamProfile::new(),
                Some(format!("{}: {e}", path.display())),
            );
        }
    };
    match toml::from_str::<StreamProfile>(&text) {
        Ok(p) => match p.problem() {
            None => (p, None),
            Some(why) => (
                StreamProfile::new(),
                Some(format!("{}: {why}", path.display())),
            ),
        },
        Err(e) => (
            StreamProfile::new(),
            Some(format!("{}: {e}", path.display())),
        ),
    }
}

/// Writes the profile (replacing the file in one step).
pub fn save(config: &Path, profile: &StreamProfile) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(profile).map_err(std::io::Error::other)?;
    let path = file(config);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::record_profile::{OutputSize, RateMode};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn servers_are_checked() {
        let ok = [
            (
                "rtmp://live.example.com/app",
                Protocol::Rtmp,
                "live.example.com",
            ),
            (
                "RTMP://a.rtmp.youtube.com/live2",
                Protocol::Rtmp,
                "a.rtmp.youtube.com",
            ),
            ("rtmp://127.0.0.1:1935/live", Protocol::Rtmp, "127.0.0.1"),
            (
                "srt://ingest.example.com:9000",
                Protocol::Srt,
                "ingest.example.com",
            ),
            ("srt://[::1]:9000", Protocol::Srt, "::1"),
        ];
        for (url, protocol, host) in ok {
            assert_eq!(
                parse_server(url),
                Ok(Server {
                    protocol,
                    host: host.to_owned()
                }),
                "{url}"
            );
        }
        let bad = [
            ("live.example.com/app", "starts with rtmp://"),
            ("rtmps://live.example.com/app", "rtmps:// is not supported"),
            ("https://x/app", "https:// is not supported"),
            ("rtmp://live.example.com", "no application name"),
            ("rtmp://live.example.com/", "no application name"),
            ("rtmp:///app", "no server name"),
            ("rtmp://x:99999/app", "not a port"),
            ("srt://ingest.example.com", "no port"),
            ("srt://[::1:9000", "not closed"),
            ("srt://x:9000?streamid=secret", "Stream key field"),
            ("rtmp://x/app?key=secret", "Stream key field"),
        ];
        for (url, why) in bad {
            let e = parse_server(url).unwrap_err();
            assert!(e.contains(why), "{url}: {e}");
            assert!(
                !e.contains("secret"),
                "the refusal never repeats a secret: {e}"
            );
        }
    }

    #[test]
    fn defaults_suit_streaming_and_say_what_is_missing() {
        let mut p = StreamProfile::new();
        assert_eq!(p.problem(), None);
        assert_eq!(p.encoding.video_kbps, 6_000);
        assert_eq!(p.encoding.rate_control, RateMode::Cbr);
        assert_eq!(p.encoding.keyframe_seconds, 2);
        assert_eq!(p.reconnect_policy(), RECONNECT);
        assert!(p.missing().unwrap().contains("server"));
        p.server = "rtmp://live.example.com/app".into();
        assert!(p.missing().unwrap().contains("stream key"));
        p.key_saved = true;
        assert_eq!(p.missing(), None);
        p.server = "srt://ingest.example.com:9000".into();
        p.key_saved = false;
        assert_eq!(p.missing(), None, "SRT's stream id is optional");
        p.server = "ftp://x".into();
        assert!(p.problem().is_some());
    }

    #[test]
    fn srt_latency_is_passed_only_to_srt() {
        let mut p = StreamProfile::new();
        p.srt_latency_ms = Some(500);
        p.server = "rtmp://live.example.com/app".into();
        assert!(p.transport_options().is_empty());
        p.server = "srt://ingest.example.com:9000".into();
        assert_eq!(
            p.transport_options(),
            [("latency_ms".to_owned(), "500".to_owned())]
        );
        p.reconnect = false;
        assert_eq!(p.reconnect_policy(), ReconnectPolicy::Off);
    }

    #[test]
    fn the_auto_record_note_says_whether_frames_are_encoded_twice_and_the_disk_use() {
        let record = crate::record_profile::RecordProfile::new().encoding();
        let stream = StreamProfile::new().encoding;
        let note = auto_record_note(&record, &stream, true);
        assert!(note.contains("encoded twice"), "{note}");
        // 12000 + 160 kbit/s for an hour: 5.47 GB.
        assert!(note.contains("about 5.5 GB"), "{note}");
        let note = auto_record_note(&stream, &stream, true);
        assert!(note.contains("share one encoder"), "{note}");
        assert!(note.contains("about 2.8 GB"), "{note}");
        // Shared as ADR-0047 shares: fields that do nothing in this mode and the audio
        // bitrate do not count; another encoder does.
        let mut other = stream.clone();
        other.quality = 30;
        other.audio_kbps = 192;
        assert!(auto_record_note(&other, &stream, true).contains("share one encoder"));
        assert!(auto_record_note(&stream, &stream, false).contains("encoded twice"));
        let mut cq = stream.clone();
        cq.rate_control = RateMode::ConstantQuality;
        let note = auto_record_note(&cq, &stream, true);
        assert!(!note.contains("GB"), "{note}");
    }

    #[test]
    fn a_saved_profile_loads_back_and_never_holds_a_key() {
        let d = dir("stream-profile");
        assert_eq!(load(&d), (StreamProfile::new(), None));
        let mut p = StreamProfile::new();
        p.server = "srt://ingest.example.com:9000".into();
        p.key_saved = true;
        p.reconnect = false;
        p.srt_latency_ms = Some(1_000);
        p.encoding.video_kbps = 8_000;
        p.encoding.rate_control = RateMode::Vbr;
        p.encoding.encoder = Some("org.rbs.codec.nvenc".into());
        p.auto_record = true;
        save(&d, &p).unwrap();
        assert_eq!(load(&d), (p.clone(), None));
        let text = std::fs::read_to_string(file(&d)).unwrap();
        assert!(text.contains("[encoding]"), "{text}");
        assert!(!text.to_lowercase().contains("secret"), "{text}");
        assert!(
            !text.contains("output_size"),
            "same as canvas is left out of the file: {text}"
        );
        p.output_size = Some(OutputSize {
            width: 1280,
            height: 720,
        });
        save(&d, &p).unwrap();
        assert_eq!(load(&d).0.output_size, p.output_size);
        let text = std::fs::read_to_string(file(&d)).unwrap();
        assert!(text.contains("width = 1280"), "{text}");
        assert!(text.contains("height = 720"), "{text}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_old_stream_file_encodes_at_its_canvas() {
        let d = dir("old-stream");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            file(&d),
            "format_version = 1\nserver = \"rtmp://live.example.com/app\"\n",
        )
        .unwrap();
        let (p, why) = load(&d);
        assert_eq!(why, None);
        assert_eq!(p.output_size, None);
        assert_eq!(p.server, "rtmp://live.example.com/app");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_control_api_sees_and_changes_all_but_the_server_and_the_key() {
        let mut p = StreamProfile::new();
        p.server = "srt://ingest.example.com:9000".into();
        p.key_saved = true;
        let api = p.to_api();
        assert_eq!(api.protocol, Some(rbs_api::types::StreamProtocol::Srt));
        let text = serde_json::to_string(&api).unwrap();
        assert!(!text.contains("ingest.example.com"), "{text}");
        let mut patch = rbs_api::types::StreamSettingsPatch {
            reconnect: false,
            srt_latency_ms: Some(1_000),
            encoding: api.encoding.clone(),
            auto_record: None,
            output_size: None,
        };
        patch.encoding.video_kbps = 9_000;
        let changed = p.with_api(&patch).unwrap();
        assert_eq!(changed.server, p.server);
        assert!(changed.key_saved);
        assert!(!changed.reconnect);
        assert_eq!(changed.srt_latency_ms, Some(1_000));
        assert_eq!(changed.encoding.video_kbps, 9_000);
        assert_eq!(changed.to_api().encoding, patch.encoding);
        // "Record when the stream starts" stays unless the patch names it.
        assert!(!changed.auto_record);
        // Omitting the size stores the same as the canvas, including over a saved one.
        p.output_size = Some(OutputSize {
            width: 1280,
            height: 720,
        });
        assert_eq!(p.with_api(&patch).unwrap().output_size, None);
        patch.output_size = Some(rbs_api::types::Dimensions {
            width: 1280,
            height: 720,
        });
        assert_eq!(
            p.with_api(&patch).unwrap().output_size,
            Some(OutputSize {
                width: 1280,
                height: 720,
            })
        );
        patch.output_size = None;
        p.output_size = None;
        patch.auto_record = Some(true);
        assert!(p.with_api(&patch).unwrap().auto_record);
        patch.auto_record = None;
        patch.encoding.video_kbps = 1;
        assert!(p.with_api(&patch).unwrap_err().contains("bitrate"));
        patch.encoding.video_kbps = 6_000;
        patch.encoding.profile = Some("high10".into());
        assert!(p.with_api(&patch).unwrap_err().contains("high10"));
        patch.encoding.profile = Some("main".into());
        patch.encoding.encoder = Some("x".repeat(200));
        assert!(p.with_api(&patch).is_err());
        patch.encoding.encoder = None;
        // A saved server that is not valid is not repeated to the caller.
        patch.encoding.video_kbps = 6_000;
        p.server = "srt://secret.example.com".into();
        let why = p.with_api(&patch).unwrap_err();
        assert!(!why.contains("secret"), "{why}");
        assert_eq!(StreamProfile::new().to_api().protocol, None);
    }

    #[test]
    fn a_bad_file_gives_the_defaults_and_why() {
        let d = dir("bad-stream-profile");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(file(&d), "format_version = 1\nserver = \"rtmps://x/app\"\n").unwrap();
        let (p, why) = load(&d);
        assert_eq!(p, StreamProfile::new());
        assert!(why.unwrap().contains("not supported"));
        std::fs::write(file(&d), "format_version = 1\nkey = \"abc\"\n").unwrap();
        assert!(load(&d).1.unwrap().contains("unknown field"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
