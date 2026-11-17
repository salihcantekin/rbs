//! The Settings search index (spec `docs/specs/settings-search.md`): every card of
//! the Settings window and every setting row worth finding, with the section and card
//! it shows in, its title as the page writes it, extra search words and its level.
//!
//! Slint has no document tree to search, and a page that is not shown has no rows at
//! all, so the index is a table kept beside the `.slint` pages. A test in `rbs-app`
//! (`settings_search_index_matches_the_ui_files`) checks it against the pages: every
//! `find:` key a row names is here and every key here is named, a static row label
//! equals its title, and the level matches the `advanced` or `expert` branch the row
//! is drawn in. The table is static data: nothing is allocated until a query runs.

/// The level a setting is drawn at (ADR-0113): `Simple` rows are always drawn,
/// `Advanced` rows at Standard and Pro, `Expert` rows at Pro, and both once the card's
/// "More options are hidden" line was answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Drawn at every level.
    Simple,
    /// Drawn at Standard and Pro.
    Advanced,
    /// Drawn at Pro.
    Expert,
}

/// Where a setting shows: a Settings section (as `ui.settings.open {section}` names
/// it) and the id of the card that holds it there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    /// The section id (`general`, `recording`, ...).
    pub section: &'static str,
    /// The card id (`settings.recording.encoding`).
    pub card: &'static str,
}

/// One entry: a card (its key is the card id) or a setting row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setting {
    /// The stable key: `ui.settings.open {setting}` and the row's `find:` property.
    pub key: &'static str,
    /// The title as the page writes it (the card title, or the row label).
    pub title: &'static str,
    /// More words a search matches (synonyms, units, other apps' names); never shown.
    pub words: &'static str,
    /// The places it shows, in section list order; shared rows (the Encoding card's)
    /// show in more than one.
    pub places: &'static [Place],
    /// Its level.
    pub level: Level,
}

impl Setting {
    /// A card entry: its key is the card id.
    pub fn is_card(&self) -> bool {
        self.key.starts_with("settings.")
    }
}

const fn at(section: &'static str, card: &'static str) -> Place {
    Place { section, card }
}

const fn row(
    key: &'static str,
    title: &'static str,
    words: &'static str,
    places: &'static [Place],
    level: Level,
) -> Setting {
    Setting {
        key,
        title,
        words,
        places,
        level,
    }
}

use Level::{Advanced, Expert, Simple};

const GENERAL_EXPERIENCE: &[Place] = &[at("general", "settings.general.experience")];
const GENERAL_CLOSING: &[Place] = &[at("general", "settings.general.closing")];
const GENERAL_CAPTURE: &[Place] = &[at("general", "settings.general.capture")];
const GENERAL_STUDIO: &[Place] = &[at("general", "settings.general.studio")];
const GENERAL_GLIDE: &[Place] = &[at("general", "settings.general.glide")];
const GENERAL_IMPORT: &[Place] = &[at("general", "settings.general.import")];
const AUDIO_MONITOR: &[Place] = &[at("audio", "settings.audio.monitor")];
const AUDIO_LOUDNESS: &[Place] = &[at("audio", "settings.audio.loudness")];
const APPEARANCE_IN_USE: &[Place] = &[at("appearance", "settings.appearance.theme-in-use")];
const APPEARANCE_THEMES: &[Place] = &[at("appearance", "settings.appearance.themes")];
const VIDEO_CANVAS: &[Place] = &[at("video", "settings.video.canvas")];
const SOURCES_LIST: &[Place] = &[at("global_sources", "settings.sources.global-sources")];
const SOURCES_ADD: &[Place] = &[at("global_sources", "settings.sources.add-global")];
const RECORDING_FILES: &[Place] = &[at("recording", "settings.recording.files")];
const RECORDING_SEPARATE: &[Place] = &[at("recording", "settings.recording.separate")];
const RECORDING_MARKERS: &[Place] = &[at("recording", "settings.recording.markers")];
const RECORDING_ENCODING: &[Place] = &[at("recording", "settings.recording.encoding")];
const RECORDING_REPLAY: &[Place] = &[at("recording", "settings.recording.replay")];
const STREAM_DESTINATION: &[Place] = &[at("stream", "settings.stream.destination")];
const STREAM_CONNECTION: &[Place] = &[at("stream", "settings.stream.connection")];
const STREAM_WHILE_LIVE: &[Place] = &[at("stream", "settings.stream.while-live")];
const STREAM_ENCODING: &[Place] = &[at("stream", "settings.stream.encoding")];
const VCAM_CAMERA: &[Place] = &[at("virtual_camera", "settings.vcam.camera")];
const VCAM_PICTURE: &[Place] = &[at("virtual_camera", "settings.vcam.picture")];
const VCAM_SENDING: &[Place] = &[at("virtual_camera", "settings.vcam.sending")];
const VCAM_REMOVAL: &[Place] = &[at("virtual_camera", "settings.vcam.removal")];
const REMOTE_THIS: &[Place] = &[at("remote_control", "settings.remote.this-computer")];
const REMOTE_OTHER: &[Place] = &[at("remote_control", "settings.remote.other-computers")];
const REMOTE_TOKENS: &[Place] = &[at("remote_control", "settings.remote.access-tokens")];
const AI_STATUS: &[Place] = &[at("ai_tools", "settings.ai-tools.status")];
const AI_ACCESS: &[Place] = &[at("ai_tools", "settings.ai-tools.access")];
const AI_CONNECT: &[Place] = &[at("ai_tools", "settings.ai-tools.connect")];
const AI_ACTIONS: &[Place] = &[at("ai_tools", "settings.ai-tools.actions")];
/// The Encoding card's rows (`EncoderFields`), shared by Recording and Stream.
const ENCODING: &[Place] = &[
    at("recording", "settings.recording.encoding"),
    at("stream", "settings.stream.encoding"),
];

/// A card entry: its key is the card id, and it shows in one place.
const fn c(
    key: &'static str,
    title: &'static str,
    words: &'static str,
    places: &'static [Place],
) -> Setting {
    row(key, title, words, places, Simple)
}

/// Every entry, in section list order (`SETTINGS_SECTIONS`), each card before its rows.
pub const SETTINGS: &[Setting] = &[
    // General
    c(
        "settings.general.experience",
        "Experience",
        "level simple standard pro beginner",
        GENERAL_EXPERIENCE,
    ),
    row(
        "general.settings-shown",
        "Settings shown",
        "experience level simple standard pro beginner advanced expert",
        GENERAL_EXPERIENCE,
        Simple,
    ),
    c(
        "settings.general.closing",
        "Closing the main window",
        "quit exit",
        GENERAL_CLOSING,
    ),
    row(
        "general.close-window",
        "When I close the main window",
        "quit exit tray background minimize keep running",
        GENERAL_CLOSING,
        Simple,
    ),
    c(
        "settings.general.capture",
        "Screen capture",
        "",
        GENERAL_CAPTURE,
    ),
    row(
        "general.hide-from-capture",
        "Leave RBS out of screen recordings",
        "screen capture exclude hide privacy screenshot",
        GENERAL_CAPTURE,
        Simple,
    ),
    c(
        "settings.general.studio",
        "Studio mode",
        "preview program",
        GENERAL_STUDIO,
    ),
    row(
        "general.swap-scenes",
        "Swap scenes after a transition",
        "studio mode preview program",
        GENERAL_STUDIO,
        Simple,
    ),
    c(
        "settings.general.glide",
        "Scene changes",
        "glide animation",
        GENERAL_GLIDE,
    ),
    row(
        "general.glide",
        "Glide",
        "scene change animation move items",
        GENERAL_GLIDE,
        Simple,
    ),
    row(
        "general.glide-duration",
        "Duration",
        "glide time speed ms",
        GENERAL_GLIDE,
        Simple,
    ),
    row(
        "general.glide-easing",
        "Easing",
        "glide curve animation",
        GENERAL_GLIDE,
        Simple,
    ),
    c(
        "settings.general.import",
        "Bring from another app",
        "import migrate obs",
        GENERAL_IMPORT,
    ),
    row(
        "general.import-obs",
        "OBS Studio",
        "import migrate obs scenes profile hotkeys switch from",
        GENERAL_IMPORT,
        Simple,
    ),
    // Audio
    c(
        "settings.audio.monitor",
        "Monitor",
        "headphones listen monitoring",
        AUDIO_MONITOR,
    ),
    row(
        "audio.monitor-device",
        "Device",
        "monitor headphones speakers output playback",
        AUDIO_MONITOR,
        Simple,
    ),
    row(
        "audio.monitor-volume",
        "Volume",
        "monitor level loudness",
        AUDIO_MONITOR,
        Simple,
    ),
    row(
        "audio.monitor-latency",
        "Latency",
        "monitor delay buffer",
        AUDIO_MONITOR,
        Simple,
    ),
    c(
        "settings.audio.loudness",
        "Loudness",
        "lufs loudness meter ebu r128 bs.1770 normalization",
        AUDIO_LOUDNESS,
    ),
    row(
        "audio.loudness-show",
        "Show loudness",
        "lufs meter master mixer",
        AUDIO_LOUDNESS,
        Simple,
    ),
    row(
        "audio.loudness-target",
        "Target",
        "lufs loudness level youtube podcast broadcast ebu r128 atsc",
        AUDIO_LOUDNESS,
        Simple,
    ),
    row(
        "audio.loudness-custom",
        "Custom target",
        "lufs loudness level",
        AUDIO_LOUDNESS,
        Simple,
    ),
    row(
        "audio.loudness-limit",
        "True-peak limit",
        "dbtp ceiling peak clipping over",
        AUDIO_LOUDNESS,
        Simple,
    ),
    // Appearance
    c(
        "settings.appearance.theme-in-use",
        "Theme in use",
        "look colours colors",
        APPEARANCE_IN_USE,
    ),
    row(
        "appearance.follow-system",
        "Use the system appearance",
        "dark light mode theme automatic",
        APPEARANCE_IN_USE,
        Simple,
    ),
    row(
        "appearance.spacing",
        "Spacing",
        "density compact roomy regular size",
        APPEARANCE_IN_USE,
        Simple,
    ),
    row(
        "appearance.compact-amount",
        "Amount",
        "compact spacing density",
        APPEARANCE_IN_USE,
        Simple,
    ),
    c(
        "settings.appearance.themes",
        "Themes",
        "theme editor import export dark light colours colors",
        APPEARANCE_THEMES,
    ),
    // Video
    c(
        "settings.video.canvas",
        "Canvas",
        "resolution base",
        VIDEO_CANVAS,
    ),
    row(
        "video.canvas-size",
        "Canvas size",
        "resolution width height 720p 1080p 1440p 4k portrait base",
        VIDEO_CANVAS,
        Simple,
    ),
    row(
        "video.frame-rate",
        "Frame rate",
        "fps framerate 24 25 30 50 60",
        VIDEO_CANVAS,
        Simple,
    ),
    // Global sources
    c(
        "settings.sources.global-sources",
        "Global sources",
        "microphone mic every scene",
        SOURCES_LIST,
    ),
    c(
        "settings.sources.add-global",
        "Add a global source",
        "microphone mic every scene",
        SOURCES_ADD,
    ),
    // Recording
    c(
        "settings.recording.files",
        "Files",
        "recording",
        RECORDING_FILES,
    ),
    row(
        "recording.folder",
        "Folder",
        "path directory location save videos movies",
        RECORDING_FILES,
        Simple,
    ),
    row(
        "recording.format",
        "Format",
        "container file type mkv mp4 mov wav flac audio only",
        RECORDING_FILES,
        Simple,
    ),
    c(
        "settings.recording.separate",
        "Separate files",
        "iso multitrack per source",
        RECORDING_SEPARATE,
    ),
    row(
        "recording.separate",
        "Record separate files",
        "iso multitrack per source",
        RECORDING_SEPARATE,
        Simple,
    ),
    row(
        "recording.separate-sources",
        "Sources",
        "iso separate files",
        RECORDING_SEPARATE,
        Simple,
    ),
    row(
        "recording.separate-video",
        "Video files",
        "iso separate files",
        RECORDING_SEPARATE,
        Simple,
    ),
    row(
        "recording.separate-sound",
        "Sound files",
        "iso separate files audio tracks",
        RECORDING_SEPARATE,
        Simple,
    ),
    row(
        "recording.separate-limit",
        "Limit",
        "iso separate files hardware encoder sessions",
        RECORDING_SEPARATE,
        Simple,
    ),
    c(
        "settings.recording.markers",
        "Markers and project",
        "marker chapter editor project resolve premiere final cut fcpxml xml timeline",
        RECORDING_MARKERS,
    ),
    row(
        "recording.editor-project",
        "Editor project",
        "davinci resolve premiere final cut fcpxml xml xmeml timeline export",
        RECORDING_MARKERS,
        Simple,
    ),
    row(
        "recording.marker-lead",
        "Marker lead",
        "marker offset reaction delay earlier seconds",
        RECORDING_MARKERS,
        Simple,
    ),
    row(
        "recording.marker-scenes",
        "Mark scene changes",
        "automatic markers scene switch chapters",
        RECORDING_MARKERS,
        Simple,
    ),
    row(
        "recording.marker-stream",
        "Mark stream start and end",
        "automatic markers live stream went live",
        RECORDING_MARKERS,
        Simple,
    ),
    row(
        "recording.marker-pauses",
        "Mark pauses",
        "automatic markers pause resume break",
        RECORDING_MARKERS,
        Simple,
    ),
    c(
        "settings.recording.encoding",
        "Encoding",
        "encoder quality bitrate",
        RECORDING_ENCODING,
    ),
    row(
        "recording.pcm",
        "PCM",
        "wav bit depth 16 24 32 float",
        RECORDING_ENCODING,
        Simple,
    ),
    row(
        "recording.flac-width",
        "Width",
        "flac bit depth 16 24",
        RECORDING_ENCODING,
        Simple,
    ),
    c(
        "settings.recording.replay",
        "Replay buffer",
        "instant replay clip save last",
        RECORDING_REPLAY,
    ),
    row(
        "replay.arm",
        "Keep the past",
        "replay buffer arm on",
        RECORDING_REPLAY,
        Simple,
    ),
    row(
        "replay.length",
        "Length",
        "replay buffer seconds minutes duration",
        RECORDING_REPLAY,
        Simple,
    ),
    row(
        "replay.memory-limit",
        "Memory limit",
        "replay buffer ram",
        RECORDING_REPLAY,
        Simple,
    ),
    row(
        "replay.sound",
        "Include sound",
        "replay buffer audio",
        RECORDING_REPLAY,
        Simple,
    ),
    row(
        "replay.arm-at-start",
        "Arm when RBS starts",
        "replay buffer automatic startup launch",
        RECORDING_REPLAY,
        Simple,
    ),
    row(
        "replay.folder",
        "Folder",
        "replay buffer path directory location save",
        RECORDING_REPLAY,
        Simple,
    ),
    // Stream
    c(
        "settings.stream.destination",
        "Destination",
        "service platform youtube twitch",
        STREAM_DESTINATION,
    ),
    row(
        "stream.server",
        "Server",
        "rtmp rtmps srt url ingest service youtube twitch",
        STREAM_DESTINATION,
        Simple,
    ),
    row(
        "stream.key",
        "Stream key",
        "secret password",
        STREAM_DESTINATION,
        Simple,
    ),
    c(
        "settings.stream.connection",
        "Connection",
        "network",
        STREAM_CONNECTION,
    ),
    row(
        "stream.reconnect",
        "Reconnect when the connection drops",
        "retry network auto reconnect",
        STREAM_CONNECTION,
        Simple,
    ),
    row(
        "stream.srt-latency",
        "SRT latency",
        "delay buffer ms",
        STREAM_CONNECTION,
        Advanced,
    ),
    c(
        "settings.stream.while-live",
        "While live",
        "",
        STREAM_WHILE_LIVE,
    ),
    row(
        "stream.record-on-start",
        "Record when the stream starts",
        "auto record automatic recording",
        STREAM_WHILE_LIVE,
        Simple,
    ),
    row(
        "stream.hold-picture",
        "While on hold",
        "hold break brb freeze last frame black picture scene",
        STREAM_WHILE_LIVE,
        Simple,
    ),
    row(
        "stream.hold-scene",
        "Hold scene",
        "hold break brb be right back scene starting soon",
        STREAM_WHILE_LIVE,
        Simple,
    ),
    row(
        "stream.hold-sound",
        "Sound while on hold",
        "hold break brb audio sound silence mute music",
        STREAM_WHILE_LIVE,
        Simple,
    ),
    c(
        "settings.stream.encoding",
        "Encoding",
        "encoder quality bitrate",
        STREAM_ENCODING,
    ),
    // The Encoding card's rows, shown in Recording and Stream.
    row(
        "encoding.output-size",
        "Output size",
        "resolution scale downscale upscale rescale 720p 1080p 1440p 4k",
        ENCODING,
        Simple,
    ),
    row(
        "encoding.video-codec",
        "Video codec",
        "h264 h.264 avc hevc h265 h.265 av1",
        ENCODING,
        Advanced,
    ),
    row(
        "encoding.colour",
        "Colour",
        "color space rec 709 srgb range full limited",
        ENCODING,
        Advanced,
    ),
    row(
        "encoding.open-shadows",
        "Open shadows",
        "dark tones black crush youtube",
        ENCODING,
        Expert,
    ),
    row(
        "encoding.video-encoder",
        "Video encoder",
        "nvenc nvidia amd amf intel quick sync qsv videotoolbox x264 software hardware gpu",
        ENCODING,
        Simple,
    ),
    row(
        "encoding.video-bitrate",
        "Video bitrate",
        "kbps kbit target bitrate quality",
        ENCODING,
        Simple,
    ),
    row(
        "encoding.quality-level",
        "Quality level",
        "crf cq cqp constant quality",
        ENCODING,
        Simple,
    ),
    row(
        "encoding.audio-bitrate",
        "Audio bitrate",
        "aac kbps kbit sound",
        ENCODING,
        Simple,
    ),
    row(
        "encoding.rate-control",
        "Rate control",
        "cbr vbr constant quality crf cq",
        ENCODING,
        Advanced,
    ),
    row(
        "encoding.max-bitrate",
        "Maximum bitrate",
        "vbr peak kbps",
        ENCODING,
        Advanced,
    ),
    row(
        "encoding.pad-bitrate",
        "Pad to the full bitrate",
        "cbr padding filler",
        ENCODING,
        Expert,
    ),
    row(
        "encoding.keyframe-interval",
        "Keyframe interval",
        "gop i-frame keyint seconds",
        ENCODING,
        Advanced,
    ),
    row(
        "encoding.encoder-preset",
        "Encoder preset",
        "speed quality balanced performance",
        ENCODING,
        Advanced,
    ),
    row(
        "encoding.profile",
        "Profile",
        "h264 high main baseline",
        ENCODING,
        Expert,
    ),
    row(
        "encoding.b-frames",
        "B-frames",
        "bframes bf",
        ENCODING,
        Expert,
    ),
    // Virtual camera
    c(
        "settings.vcam.camera",
        "Camera",
        "virtual camera webcam",
        VCAM_CAMERA,
    ),
    row(
        "vcam.enable",
        "Virtual camera",
        "webcam zoom teams meet meeting camera add install",
        VCAM_CAMERA,
        Simple,
    ),
    c(
        "settings.vcam.picture",
        "Picture",
        "virtual camera webcam",
        VCAM_PICTURE,
    ),
    row(
        "vcam.size",
        "Size",
        "virtual camera resolution width height",
        VCAM_PICTURE,
        Simple,
    ),
    row(
        "vcam.frame-rate",
        "Frame rate",
        "virtual camera fps framerate",
        VCAM_PICTURE,
        Simple,
    ),
    c(
        "settings.vcam.sending",
        "Sending",
        "virtual camera start stop",
        VCAM_SENDING,
    ),
    c(
        "settings.vcam.removal",
        "Removal",
        "virtual camera uninstall delete driver",
        VCAM_REMOVAL,
    ),
    row(
        "vcam.remove",
        "Remove completely",
        "virtual camera uninstall delete driver",
        VCAM_REMOVAL,
        Simple,
    ),
    // Remote control
    c(
        "settings.remote.this-computer",
        "This computer",
        "remote control api",
        REMOTE_THIS,
    ),
    row(
        "remote.allow",
        "Allow remote control",
        "api stream deck websocket ipc control scripts",
        REMOTE_THIS,
        Simple,
    ),
    row(
        "remote.port",
        "Port",
        "remote control api websocket network",
        REMOTE_THIS,
        Simple,
    ),
    c(
        "settings.remote.other-computers",
        "Other computers",
        "remote control network lan",
        REMOTE_OTHER,
    ),
    row(
        "remote.allow-network",
        "Allow access from other computers",
        "remote control lan network wifi",
        REMOTE_OTHER,
        Simple,
    ),
    row(
        "remote.key-pin",
        "Key pin",
        "remote control tls certificate trust",
        REMOTE_OTHER,
        Simple,
    ),
    row(
        "remote.fingerprint",
        "Certificate fingerprint (SHA-256)",
        "remote control tls certificate trust",
        REMOTE_OTHER,
        Simple,
    ),
    row(
        "remote.allow-admin",
        "Allow remote admin",
        "remote control admin rights scope",
        REMOTE_OTHER,
        Simple,
    ),
    row(
        "remote.certificate",
        "Certificate",
        "remote control tls https",
        REMOTE_OTHER,
        Simple,
    ),
    row(
        "remote.certificate-file",
        "Certificate file",
        "remote control tls pem",
        REMOTE_OTHER,
        Simple,
    ),
    row(
        "remote.key-file",
        "Key file",
        "remote control tls pem private",
        REMOTE_OTHER,
        Simple,
    ),
    row(
        "remote.new-key",
        "New key",
        "remote control tls regenerate rotate",
        REMOTE_OTHER,
        Simple,
    ),
    c(
        "settings.remote.access-tokens",
        "Access tokens",
        "remote control api token password scope",
        REMOTE_TOKENS,
    ),
    // AI tools (ADR-0134)
    c(
        "settings.ai-tools.status",
        "AI tools",
        "mcp claude cursor agent llm assistant",
        AI_STATUS,
    ),
    row(
        "ai-tools.start",
        "Start",
        "mcp claude cursor agent stop server",
        AI_STATUS,
        Simple,
    ),
    row(
        "ai-tools.autostart",
        "Start when RBS opens",
        "mcp agent automatic launch login",
        AI_STATUS,
        Simple,
    ),
    c(
        "settings.ai-tools.access",
        "What AI tools may do",
        "mcp agent permission scope",
        AI_ACCESS,
    ),
    row(
        "ai-tools.access",
        "Access",
        "mcp agent scope view control full",
        AI_ACCESS,
        Simple,
    ),
    row(
        "ai-tools.irreversible",
        "Allow actions that cannot be undone",
        "mcp agent permission record stream delete irreversible",
        AI_ACCESS,
        Simple,
    ),
    row(
        "ai-tools.new-token",
        "Token",
        "mcp agent token rotate new key",
        AI_ACCESS,
        Simple,
    ),
    c(
        "settings.ai-tools.connect",
        "Connect an AI tool",
        "mcp claude desktop code cursor vs code config json",
        AI_CONNECT,
    ),
    row(
        "ai-tools.tool",
        "AI tool",
        "mcp claude desktop code cursor vs code",
        AI_CONNECT,
        Simple,
    ),
    row(
        "ai-tools.line",
        "Connection line",
        "mcp config json command copy rbs-mcp",
        AI_CONNECT,
        Simple,
    ),
    c(
        "settings.ai-tools.actions",
        "Recent actions",
        "mcp agent audit log history",
        AI_ACTIONS,
    ),
];

/// The entry with this key.
pub fn find(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.key == key)
}

/// The place of `key` in `section`, or its first place without a section; `None` for
/// an unknown key or a section it does not show in.
pub fn place(key: &str, section: Option<&str>) -> Option<Place> {
    let s = find(key)?;
    match section {
        Some(name) => s.places.iter().find(|p| p.section == name).copied(),
        None => s.places.first().copied(),
    }
}

/// One search result: an entry in one of its places.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    /// The entry found.
    pub setting: &'static Setting,
    /// The place it was found in.
    pub place: Place,
}

impl Hit {
    /// The card's title for a row, `None` for a card.
    pub fn card_title(&self) -> Option<&'static str> {
        if self.setting.is_card() {
            return None;
        }
        find(self.place.card).map(|c| c.title)
    }
}

/// The entries whose words hold every word of `query` (case-insensitive), one hit per
/// place. A hit's own words are its section's title, its card's title, its own title
/// and its extra words; `context` (the RBS menu passes "File Settings") also counts,
/// but only for an entry that some query word finds in its own words, so "settings"
/// alone matches no entry and "settings encoder" finds the encoder. Hits whose own
/// title holds every word come first, then the rest, each group in index order. None
/// for a blank query. `section_title` names a section
/// (`rbs_ui::command::settings_section_title`).
pub fn search(query: &str, context: &str, section_title: &dyn Fn(&str) -> String) -> Vec<Hit> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Vec::new();
    }
    let context = context.to_lowercase();
    let mut first = Vec::new();
    let mut rest = Vec::new();
    for s in SETTINGS {
        let own = s.title.to_lowercase();
        for p in s.places {
            let hit = Hit {
                setting: s,
                place: *p,
            };
            let card = hit.card_title().unwrap_or("");
            let text =
                format!("{} {card} {own} {}", section_title(p.section), s.words).to_lowercase();
            let in_text = |w: &String| text.contains(w.as_str());
            if !words.iter().any(in_text)
                || !words
                    .iter()
                    .all(|w| in_text(w) || context.contains(w.as_str()))
            {
                continue;
            }
            if words.iter().all(|w| own.contains(w.as_str())) {
                first.push(hit);
            } else {
                rest.push(hit);
            }
        }
    }
    first.extend(rest);
    first
}

/// One result of the Settings window's search field (step 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    /// A section whose title holds every word.
    Section(&'static str),
    /// A card or setting.
    Setting(Hit),
    /// A command of the registry, shown in Shortcuts: its id and title.
    Command(&'static str, &'static str),
}

/// Most settings and commands [`window_search`] lists.
pub const MAX_WINDOW_SETTINGS: usize = 30;
/// Most commands [`window_search`] lists, after the settings.
pub const MAX_WINDOW_COMMANDS: usize = 8;

/// The Settings window's search: sections whose title holds every word, then the
/// index hits ([`search`], at most [`MAX_WINDOW_SETTINGS`]), then registry commands
/// whose title holds every word (at most [`MAX_WINDOW_COMMANDS`]), which open in
/// Shortcuts. None for a blank query.
pub fn window_search(query: &str, section_title: &dyn Fn(&str) -> String) -> Vec<Found> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Vec::new();
    }
    let all = |text: &str| {
        let text = text.to_lowercase();
        words.iter().all(|w| text.contains(w.as_str()))
    };
    let mut out: Vec<Found> = crate::command::SETTINGS_SECTIONS
        .iter()
        .filter(|s| all(&section_title(s)))
        .map(|s| Found::Section(s))
        .collect();
    out.extend(
        search(query, "", section_title)
            .into_iter()
            .take(MAX_WINDOW_SETTINGS)
            .map(Found::Setting),
    );
    out.extend(
        crate::command::ENTRIES
            .iter()
            .filter(|e| all(e.title))
            .take(MAX_WINDOW_COMMANDS)
            .map(|e| Found::Command(e.id, e.title)),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{SETTINGS_SECTIONS, settings_section_title};

    fn titles(query: &str) -> Vec<(String, &'static str)> {
        search(query, "", &settings_section_title)
            .into_iter()
            .map(|h| (h.place.section.to_owned(), h.setting.key))
            .collect()
    }

    #[test]
    fn keys_are_unique_and_places_are_known() {
        for (i, s) in SETTINGS.iter().enumerate() {
            assert!(
                SETTINGS[..i].iter().all(|o| o.key != s.key),
                "{} twice",
                s.key
            );
            assert!(!s.places.is_empty(), "{} has no place", s.key);
            for p in s.places {
                assert!(
                    SETTINGS_SECTIONS.contains(&p.section),
                    "{}: {}",
                    s.key,
                    p.section
                );
                let card = find(p.card).unwrap_or_else(|| panic!("{}: no card {}", s.key, p.card));
                assert!(card.is_card());
                assert_eq!(card.places, &[*p][..], "{}: card {}", s.key, p.card);
            }
            if s.is_card() {
                assert_eq!(s.places.len(), 1, "{}", s.key);
                assert_eq!(s.places[0].card, s.key);
                assert_eq!(s.level, Level::Simple);
            }
        }
    }

    #[test]
    fn entries_follow_the_section_list() {
        let order = |s: &str| SETTINGS_SECTIONS.iter().position(|x| *x == s);
        let firsts: Vec<_> = SETTINGS
            .iter()
            .filter(|s| s.is_card())
            .map(|s| order(s.places[0].section))
            .collect();
        let mut sorted = firsts.clone();
        sorted.sort();
        assert_eq!(firsts, sorted);
    }

    #[test]
    fn search_matches_every_word_and_puts_title_matches_first() {
        assert!(titles("").is_empty());
        assert!(titles("   ").is_empty());
        // "encoder" is in the Video encoder title; the Encoding cards only match by
        // their extra words, so they follow.
        let r = titles("ENCODER");
        assert_eq!(r[0], ("recording".to_owned(), "encoding.video-encoder"));
        assert_eq!(r[1], ("stream".to_owned(), "encoding.video-encoder"));
        assert!(
            r.iter().position(|h| h.1 == "encoding.encoder-preset")
                < r.iter().position(|h| h.1 == "settings.recording.encoding")
        );
        // Section and card titles count: "replay folder" finds the buffer's folder,
        // not the recording folder.
        assert_eq!(
            titles("replay folder"),
            vec![("recording".to_owned(), "replay.folder")]
        );
        // Extra words count.
        assert_eq!(titles("nvenc"), titles("videotoolbox"));
        assert!(titles("no such thing").is_empty());
        // The context counts only beside a word the entry has itself: "file" finds
        // the settings about files, not every entry through the menu's "File".
        let hits = |q: &str| search(q, "File Settings", &settings_section_title).len();
        for q in ["file", "settings"] {
            let own: usize = search(q, "", &settings_section_title).len();
            assert_eq!(hits(q), own, "{q}");
            assert!(own < 20, "{q}: {own}");
        }
        assert_eq!(hits("settings keyframe"), 2);
    }

    #[test]
    fn the_window_lists_sections_settings_then_commands() {
        assert!(window_search(" ", &settings_section_title).is_empty());
        let found = window_search("record", &settings_section_title);
        assert_eq!(found[0], Found::Section("recording"));
        let first_command = found
            .iter()
            .position(|f| matches!(f, Found::Command(..)))
            .unwrap_or(found.len());
        assert!(
            found[1..first_command]
                .iter()
                .all(|f| matches!(f, Found::Setting(_)))
        );
        assert!(
            found[first_command..]
                .iter()
                .all(|f| matches!(f, Found::Command(..)))
        );
        assert!(first_command < found.len(), "a record command");
        assert!(found.len() <= 1 + MAX_WINDOW_SETTINGS + MAX_WINDOW_COMMANDS);
        let found = window_search("keyframe", &settings_section_title);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn place_follows_the_section() {
        let p = place("encoding.b-frames", Some("stream"));
        assert_eq!(p.map(|p| p.card), Some("settings.stream.encoding"));
        assert_eq!(
            place("encoding.b-frames", None).map(|p| p.card),
            Some("settings.recording.encoding")
        );
        assert_eq!(place("encoding.b-frames", Some("general")), None);
        assert_eq!(place("nope", None), None);
    }

    #[test]
    fn hit_names_its_card() {
        let h = search("keyframe", "", &settings_section_title);
        assert_eq!(h[0].card_title(), Some("Encoding"));
        let h = search("replay buffer", "", &settings_section_title);
        assert_eq!(h[0].setting.key, "settings.recording.replay");
        assert_eq!(h[0].card_title(), None);
    }
}
