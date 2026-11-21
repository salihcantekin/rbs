//! The destinations of each canvas's stream (ADR-0127 decision 1): where one stream is
//! sent besides, or instead of, its first destination. Kept in the output profile's own
//! `[stream-destinations]` section with its own format version, because `stream` and
//! `stream-vertical` deny unknown fields in older builds.
//!
//! The first destination of each canvas (id [`MAIN_FIRST`] or [`VERTICAL_FIRST`]) is the
//! server, key, reconnect and SRT latency of `stream` or `stream-vertical`, so an older
//! profile is a one-destination stream and reads unchanged. Its name, service, enabled
//! flag and encoding mode are kept here. It exists while its stream section has a
//! server ([`first_exists`], ADR-0148 decision 2); removing it empties the section, and
//! the next destination added to that canvas fills the slot again (decision 4).
//! Every other destination is kept here whole, apart from its key, which lives in the
//! credential store under [`key_account`]. Ids are unique across the profile and never
//! reused ([`Destinations::next_id`]), apart from the two fixed first ids, so a removed
//! destination's key can never attach to a new one. Streaming is available on a canvas
//! while one of its existing destinations is on ([`Destinations::available`]).
//! Toolkit-free and tested without a window.

use rbs_transport::ReconnectPolicy;
use serde::{Deserialize, Serialize};

use crate::record_profile::{Encoding, OutputSize, output_size_problem};
use crate::stream_profile::{Protocol, RECONNECT, Server, StreamProfile, parse_server};

/// The output profile section that holds [`Destinations`].
pub const SECTION: &str = "stream-destinations";

/// The section's own format version; a higher one was written by a newer RBS.
pub const FORMAT_VERSION: u32 = 1;

/// Destinations one canvas's stream has at most, its first one included (ADR-0127
/// decision 1). With the recording this is `rbs_record::MAX_OUTPUTS_PER_CANVAS`.
pub const MAX_DESTINATIONS_PER_CANVAS: usize = 6;

/// Destinations of one canvas with their own encoding at most (ADR-0127 decision 2):
/// with the recording's and the stream's shared one, `rbs_record::MAX_CHAINS_PER_CANVAS`.
pub const MAX_OWN_ENCODINGS_PER_CANVAS: usize = 2;

/// The id of the main canvas's first destination (`stream`).
pub const MAIN_FIRST: u32 = 0;
/// The id of the vertical canvas's first destination (`stream-vertical`).
pub const VERTICAL_FIRST: u32 = 1;

/// The service of a destination with no preset.
pub const CUSTOM: &str = "custom";

/// The canvas a destination's stream comes from (ADR-0127 decision 15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Canvas {
    /// The main canvas (`stream`).
    Main,
    /// The vertical canvas (`stream-vertical`).
    Vertical,
}

impl Canvas {
    /// The id of this canvas's first destination.
    pub fn first(self) -> u32 {
        match self {
            Canvas::Main => MAIN_FIRST,
            Canvas::Vertical => VERTICAL_FIRST,
        }
    }
}

/// How a destination is encoded (ADR-0127 decision 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum DestinationEncoding {
    /// The canvas stream's encoding, shared with every other such destination.
    #[default]
    Shared,
    /// Its own encoder chain: another bitrate or output size.
    Own {
        /// The encoder settings.
        encoding: Encoding,
        /// Encoded picture size; absent: the canvas.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_size: Option<OutputSize>,
    },
}

impl DestinationEncoding {
    /// True for [`DestinationEncoding::Own`].
    pub fn is_own(&self) -> bool {
        matches!(self, DestinationEncoding::Own { .. })
    }
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

/// One destination as saved. For a first destination (id 0 or 1) the server, key flag,
/// reconnect and SRT latency are those of `stream` or `stream-vertical`: the fields
/// here are ignored and not written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Destination {
    /// Unique across the profile, never reused.
    pub id: u32,
    /// The canvas whose stream it gets.
    pub canvas: Canvas,
    /// The name the user sees ("YouTube", "Twitch backup").
    #[serde(default)]
    pub name: String,
    /// The service preset's id, or [`CUSTOM`]. Data only: the engine never sees it.
    #[serde(default = "custom")]
    pub service: String,
    /// Go live sends to it. Enabling one is a separate, visible step.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// The server URL; never holds the key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub server: String,
    /// A key was saved in the credential store under [`key_account`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub key_saved: bool,
    /// Reconnect when the connection drops ([`RECONNECT`]).
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub reconnect: bool,
    /// SRT latency in ms; `None`: the transport's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srt_latency_ms: Option<u16>,
    /// Shared with the stream, or its own.
    #[serde(default)]
    pub encoding: DestinationEncoding,
    /// Its place in the list (ADR-0148 decision 5); `None` in sections written before
    /// it: those keep their place in id order, the first slot first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<u32>,
}

fn custom() -> String {
    CUSTOM.to_owned()
}

impl Destination {
    /// A new destination of `canvas`, enabled, sharing the stream's encoding, with no
    /// server yet.
    pub fn new(id: u32, canvas: Canvas, name: &str, service: &str) -> Self {
        Self {
            id,
            canvas,
            name: name.to_owned(),
            service: service.to_owned(),
            enabled: true,
            server: String::new(),
            key_saved: false,
            reconnect: true,
            srt_latency_ms: None,
            encoding: DestinationEncoding::Shared,
            order: None,
        }
    }

    /// It is its canvas's first destination (`stream` or `stream-vertical`).
    pub fn is_first(&self) -> bool {
        self.id == self.canvas.first()
    }
}

/// The `[stream-destinations]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Destinations {
    /// The section's format version.
    pub format_version: u32,
    /// The next id to give; ids are never reused.
    pub next_id: u32,
    /// The destinations' own entries, first destinations included when the user named
    /// or disabled them.
    #[serde(default, rename = "destination", skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<Destination>,
}

impl Default for Destinations {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            next_id: VERTICAL_FIRST + 1,
            entries: Vec::new(),
        }
    }
}

/// A destination ready to show or start: a saved entry with its first destination's
/// fields filled from `stream` or `stream-vertical`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The destination; for a first one, its server, key flag, reconnect and SRT
    /// latency are its stream section's.
    pub destination: Destination,
    /// Its credential account ([`key_account`]).
    pub account: String,
}

impl Resolved {
    /// The server taken apart, `None` when empty or not valid.
    pub fn parsed(&self) -> Option<Server> {
        parse_server(&self.destination.server).ok()
    }

    /// It has a server and, when the protocol needs one, a saved key.
    pub fn configured(&self) -> bool {
        match self.parsed() {
            Some(s) => !s.protocol.needs_key() || self.destination.key_saved,
            None => false,
        }
    }

    /// The reconnect policy.
    pub fn reconnect_policy(&self) -> ReconnectPolicy {
        if self.destination.reconnect {
            RECONNECT
        } else {
            ReconnectPolicy::Off
        }
    }

    /// Opaque transport options: the SRT latency when one is set and the server is SRT.
    pub fn transport_options(&self) -> Vec<(String, String)> {
        match (
            self.parsed().map(|s| s.protocol),
            self.destination.srt_latency_ms,
        ) {
            (Some(Protocol::Srt), Some(ms)) => vec![("latency_ms".to_owned(), ms.to_string())],
            _ => Vec::new(),
        }
    }
}

/// The credential account of destination `id`'s key, beside the profile's own account
/// `main_account` (`stream_key::account`): the first destinations keep today's
/// accounts, every other one `<main_account>/dest-<id>` (ADR-0127 decision 1).
pub fn key_account(main_account: &str, id: u32) -> String {
    match id {
        MAIN_FIRST => main_account.to_owned(),
        VERTICAL_FIRST => crate::stream_key::vertical_account(main_account),
        _ => format!("{main_account}/dest-{id}"),
    }
}

/// The first destination of a canvas exists while its stream section has a server
/// (ADR-0148 decision 2). A saved key alone does not make it exist.
pub fn first_exists(stream: &StreamProfile) -> bool {
    !stream.server.trim().is_empty()
}

impl Destinations {
    /// The destinations a saved section gives: none when it is absent or does not load.
    /// `Err` when it was written by a newer RBS: it is then read as none and must not be
    /// overwritten.
    pub fn from_section(saved: Result<Option<Destinations>, String>) -> Result<Self, String> {
        match saved {
            Ok(Some(d)) if d.format_version > FORMAT_VERSION => Err(format!(
                "stream destinations were saved by a newer RBS (format {}), so this version \
                 streams to the first destination only and does not change them",
                d.format_version
            )),
            Ok(Some(d)) => match d.problem() {
                None => Ok(d),
                Some(why) => Err(format!("stream destinations: {why}")),
            },
            Ok(None) => Ok(Self::default()),
            Err(why) => Err(format!("stream destinations: {why}")),
        }
    }

    /// Why this section cannot be used, in words, or `None`.
    pub fn problem(&self) -> Option<String> {
        let mut seen = std::collections::BTreeSet::new();
        for d in &self.entries {
            if !seen.insert(d.id) {
                return Some(format!("destination id {} is used twice", d.id));
            }
            if d.id >= self.next_id {
                return Some(format!(
                    "destination id {} is not below next_id {}",
                    d.id, self.next_id
                ));
            }
            if (d.id == MAIN_FIRST || d.id == VERTICAL_FIRST) && !d.is_first() {
                return Some(format!(
                    "destination {} is the first of the other canvas",
                    d.id
                ));
            }
            if !d.is_first()
                && !d.server.is_empty()
                && let Err(why) = parse_server(&d.server)
            {
                return Some(why);
            }
            if d.srt_latency_ms == Some(0) {
                return Some("An SRT latency of 0 ms is not possible.".into());
            }
            if let DestinationEncoding::Own {
                encoding,
                output_size,
            } = &d.encoding
                && let Some(why) = encoding
                    .problem()
                    .or_else(|| output_size_problem(*output_size))
            {
                return Some(why);
            }
        }
        for canvas in [Canvas::Main, Canvas::Vertical] {
            let of = |d: &&Destination| d.canvas == canvas;
            let others = self.entries.iter().filter(of).filter(|d| !d.is_first());
            if others.count() + 1 > MAX_DESTINATIONS_PER_CANVAS {
                return Some(format!(
                    "a stream has at most {MAX_DESTINATIONS_PER_CANVAS} destinations"
                ));
            }
            let own = self
                .entries
                .iter()
                .filter(of)
                .filter(|d| d.encoding.is_own())
                .count();
            if own > MAX_OWN_ENCODINGS_PER_CANVAS {
                return Some(format!(
                    "a stream has at most {MAX_OWN_ENCODINGS_PER_CANVAS} destinations with \
                     their own encoding"
                ));
            }
        }
        None
    }

    /// The existing destinations of `canvas` in list order (ADR-0148 decision 5): its
    /// first one (from `stream`, that canvas's stream section) only while it exists,
    /// and the others. `main_account` is the profile's key account
    /// (`stream_key::account`).
    pub fn of(&self, canvas: Canvas, stream: &StreamProfile, main_account: &str) -> Vec<Resolved> {
        let first_id = canvas.first();
        let first = first_exists(stream).then(|| {
            let mut first = self
                .entries
                .iter()
                .find(|d| d.id == first_id)
                .cloned()
                .unwrap_or_else(|| Destination::new(first_id, canvas, "", CUSTOM));
            first.server = stream.server.clone();
            first.key_saved = stream.key_saved;
            first.reconnect = stream.reconnect;
            first.srt_latency_ms = stream.srt_latency_ms;
            first
        });
        let mut list: Vec<Destination> = first
            .into_iter()
            .chain(
                self.entries
                    .iter()
                    .filter(|d| d.canvas == canvas && !d.is_first())
                    .cloned(),
            )
            .collect();
        // An entry without an order (written before ADR-0148 decision 5) has its
        // place in id order, the first slot first; every new one gets a higher order.
        list.sort_by_key(|d| (!d.is_first(), d.id));
        let mut keyed: Vec<(u32, Destination)> = list
            .into_iter()
            .enumerate()
            .map(|(i, d)| (d.order.unwrap_or(u32::try_from(i).unwrap_or(u32::MAX)), d))
            .collect();
        keyed.sort_by_key(|(o, d)| (*o, d.id));
        keyed
            .into_iter()
            .map(|(_, destination)| Resolved {
                account: key_account(main_account, destination.id),
                destination,
            })
            .collect()
    }

    /// Streaming is available on `canvas`: one of its existing destinations is on
    /// (ADR-0148 decision 1). The caller treats a section that failed to load as
    /// available.
    pub fn available(&self, canvas: Canvas, stream: &StreamProfile) -> bool {
        self.of(canvas, stream, "")
            .iter()
            .any(|r| r.destination.enabled)
    }

    /// The order a destination added to `canvas` now gets: after every one, listed
    /// or not (an entry without an order counts by its place, so at most the count).
    pub fn next_order(&self, canvas: Canvas, stream: &StreamProfile) -> u32 {
        let of_canvas = self.entries.iter().filter(|d| d.canvas == canvas);
        let top = of_canvas.clone().filter_map(|d| d.order).max().unwrap_or(0);
        let places = u32::try_from(of_canvas.count() + usize::from(first_exists(stream)))
            .unwrap_or(u32::MAX);
        top.max(places).saturating_add(1)
    }

    /// The entry of `id`, if saved here.
    pub fn get(&self, id: u32) -> Option<&Destination> {
        self.entries.iter().find(|d| d.id == id)
    }

    /// Adds a destination to `canvas` and returns its id. Refused, in words, at the
    /// limit.
    pub fn add(&mut self, canvas: Canvas, name: &str, service: &str) -> Result<u32, String> {
        let others = self
            .entries
            .iter()
            .filter(|d| d.canvas == canvas && !d.is_first())
            .count();
        if others + 1 >= MAX_DESTINATIONS_PER_CANVAS {
            return Err(format!(
                "A stream has at most {MAX_DESTINATIONS_PER_CANVAS} destinations."
            ));
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or("no destination id is left")?;
        self.entries
            .push(Destination::new(id, canvas, name, service));
        Ok(id)
    }

    /// Removes destination `id`'s entry and returns it (its key is the caller's to
    /// delete). For a first destination the entry may be missing (`Ok(None)`); the
    /// caller empties its stream section (ADR-0148 decision 3).
    pub fn remove(&mut self, id: u32) -> Result<Option<Destination>, String> {
        if id == MAIN_FIRST || id == VERTICAL_FIRST {
            let at = self.entries.iter().position(|d| d.id == id);
            return Ok(at.map(|at| self.entries.remove(at)));
        }
        let at = self
            .entries
            .iter()
            .position(|d| d.id == id)
            .ok_or_else(|| format!("there is no destination {id}"))?;
        Ok(Some(self.entries.remove(at)))
    }

    /// The entry of `id`, made for a first destination when it has none yet. `None`
    /// for an unknown id that is not a first destination.
    pub fn entry_mut(&mut self, id: u32) -> Option<&mut Destination> {
        let canvas = match id {
            MAIN_FIRST => Some(Canvas::Main),
            VERTICAL_FIRST => Some(Canvas::Vertical),
            _ => None,
        };
        if self.get(id).is_none() {
            let canvas = canvas?;
            self.entries
                .insert(0, Destination::new(id, canvas, "", CUSTOM));
        }
        self.entries.iter_mut().find(|d| d.id == id)
    }

    /// Sets whether Go live uses `id`. `false` when there is no such destination.
    pub fn set_enabled(&mut self, id: u32, enabled: bool) -> bool {
        match self.entry_mut(id) {
            Some(d) => {
                d.enabled = enabled;
                true
            }
            None => false,
        }
    }

    /// Sets how `id` is encoded. Refused, in words, past
    /// [`MAX_OWN_ENCODINGS_PER_CANVAS`] or for an unknown id.
    pub fn set_encoding(&mut self, id: u32, encoding: DestinationEncoding) -> Result<(), String> {
        let canvas = match id {
            MAIN_FIRST => Canvas::Main,
            VERTICAL_FIRST => Canvas::Vertical,
            _ => self
                .get(id)
                .map(|d| d.canvas)
                .ok_or_else(|| format!("there is no destination {id}"))?,
        };
        if encoding.is_own() {
            let own = self
                .entries
                .iter()
                .filter(|d| d.canvas == canvas && d.id != id && d.encoding.is_own())
                .count();
            if own >= MAX_OWN_ENCODINGS_PER_CANVAS {
                return Err(format!(
                    "A stream has at most {MAX_OWN_ENCODINGS_PER_CANVAS} destinations with their \
                     own encoding."
                ));
            }
        }
        if let Some(d) = self.entry_mut(id) {
            d.encoding = encoding;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn stream(server: &str, key: bool) -> StreamProfile {
        let mut s = StreamProfile::new();
        s.server = server.to_owned();
        s.key_saved = key;
        s
    }

    #[test]
    fn without_a_section_each_canvas_has_its_first_destination_only() {
        let d = Destinations::from_section(Ok(None)).unwrap();
        let main = d.of(
            Canvas::Main,
            &stream("rtmp://live.example.com/app", true),
            "stream key",
        );
        assert_eq!(main.len(), 1);
        let first = &main[0];
        assert_eq!(first.destination.id, MAIN_FIRST);
        assert!(first.destination.enabled);
        assert_eq!(first.destination.server, "rtmp://live.example.com/app");
        assert!(first.configured());
        assert_eq!(first.account, "stream key");
        let vertical = d.of(
            Canvas::Vertical,
            &stream("srt://v.example.com:9000", false),
            "stream key",
        );
        assert_eq!(vertical[0].destination.id, VERTICAL_FIRST);
        assert_eq!(vertical[0].account, "stream key/vertical");
        // No server: the first slot does not exist and streaming is off (ADR-0148).
        assert!(d.of(Canvas::Vertical, &stream("", true), "k").is_empty());
        assert!(!d.available(Canvas::Vertical, &stream("", true)));
        assert!(d.available(Canvas::Main, &stream("rtmp://a/b", false)));
    }

    #[test]
    fn a_removed_first_slot_is_not_listed_and_the_others_keep_streaming_available() {
        let mut d = Destinations::default();
        let a = d.add(Canvas::Main, "Twitch", "twitch").unwrap();
        let empty = stream("", false);
        let list = d.of(Canvas::Main, &empty, "k");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].destination.id, a);
        assert!(d.available(Canvas::Main, &empty));
        // Every one off: not available, still listed.
        assert!(d.set_enabled(a, false));
        assert!(!d.available(Canvas::Main, &empty));
        assert_eq!(d.of(Canvas::Main, &empty, "k").len(), 1);
        // A first slot that exists but is off counts as off too.
        let full = stream("rtmp://a/b", true);
        assert!(d.set_enabled(MAIN_FIRST, false));
        assert!(!d.available(Canvas::Main, &full));
    }

    #[test]
    fn saved_order_lists_a_refilled_first_slot_last() {
        let mut d = Destinations::default();
        let a = d.add(Canvas::Main, "Twitch", "twitch").unwrap();
        let b = d.add(Canvas::Main, "Kick", "kick").unwrap();
        let empty = stream("", false);
        let ids = |d: &Destinations, s: &StreamProfile| -> Vec<u32> {
            d.of(Canvas::Main, s, "")
                .iter()
                .map(|r| r.destination.id)
                .collect()
        };
        // Sections from before the order: by id, the first slot first.
        let full = stream("rtmp://a/b", true);
        assert_eq!(ids(&d, &full), vec![MAIN_FIRST, a, b]);
        // The slot refilled while empty gets the next order and lists last.
        let order = d.next_order(Canvas::Main, &empty);
        d.entry_mut(MAIN_FIRST).unwrap().order = Some(order);
        assert_eq!(ids(&d, &full), vec![a, b, MAIN_FIRST]);
        assert!(order > 2);
    }

    #[test]
    fn ids_are_never_reused_and_first_destinations_stay() {
        let mut d = Destinations::default();
        let a = d.add(Canvas::Main, "Twitch", "twitch").unwrap();
        let b = d.add(Canvas::Vertical, "Shorts", "youtube").unwrap();
        assert_eq!((a, b), (2, 3));
        d.remove(a).unwrap();
        let c = d.add(Canvas::Main, "Kick", "kick").unwrap();
        assert_eq!(c, 4);
        // A first slot is cleared, not refused (ADR-0148 decision 3); with no entry
        // there is nothing to take out of this section.
        assert_eq!(d.remove(MAIN_FIRST), Ok(None));
        assert_eq!(d.remove(VERTICAL_FIRST), Ok(None));
        assert!(d.remove(a).is_err());
        assert_eq!(key_account("stream-key/p7", c), "stream-key/p7/dest-4");
        assert_eq!(key_account("stream-key/p7", MAIN_FIRST), "stream-key/p7");
        assert_eq!(
            key_account("stream-key/p7", VERTICAL_FIRST),
            "stream-key/p7/vertical"
        );
    }

    #[test]
    fn limits_are_kept_per_canvas() {
        let mut d = Destinations::default();
        for i in 1..MAX_DESTINATIONS_PER_CANVAS {
            d.add(Canvas::Main, &format!("d{i}"), CUSTOM).unwrap();
        }
        assert!(d.add(Canvas::Main, "one more", CUSTOM).is_err());
        // The other canvas has its own list.
        let v = d.add(Canvas::Vertical, "v", CUSTOM).unwrap();
        let own = || DestinationEncoding::Own {
            encoding: Encoding::new(3_000),
            output_size: None,
        };
        d.set_encoding(MAIN_FIRST, own()).unwrap();
        d.set_encoding(2, own()).unwrap();
        assert!(d.set_encoding(3, own()).is_err());
        // Setting the same one again is not one more.
        d.set_encoding(2, own()).unwrap();
        d.set_encoding(v, own()).unwrap();
        assert_eq!(d.problem(), None);
        assert!(d.set_encoding(99, DestinationEncoding::Shared).is_err());
    }

    #[test]
    fn the_first_destination_keeps_its_name_and_flag_here_and_its_server_in_stream() {
        let mut d = Destinations::default();
        assert!(d.set_enabled(MAIN_FIRST, false));
        d.entry_mut(MAIN_FIRST).unwrap().name = "YouTube".into();
        // A server written here for a first destination is not used.
        d.entry_mut(MAIN_FIRST).unwrap().server = "rtmp://elsewhere/app".into();
        let twitch = d.add(Canvas::Main, "Twitch", "twitch").unwrap();
        assert!(!d.set_enabled(99, false));
        let main = d.of(
            Canvas::Main,
            &stream("rtmp://a.rtmp.youtube.com/live2", true),
            "stream key",
        );
        assert_eq!(main.len(), 2);
        assert_eq!(main[0].destination.name, "YouTube");
        assert!(!main[0].destination.enabled);
        assert_eq!(
            main[0].destination.server,
            "rtmp://a.rtmp.youtube.com/live2"
        );
        assert_eq!(main[1].destination.id, twitch);
        assert_eq!(main[1].account, "stream key/dest-2");
        assert!(!main[1].configured(), "no server yet");
    }

    #[test]
    fn the_section_round_trips_through_toml() {
        let mut d = Destinations::default();
        d.set_enabled(MAIN_FIRST, false);
        let id = d.add(Canvas::Main, "Twitch", "twitch").unwrap();
        {
            let e = d.entry_mut(id).unwrap();
            e.server = "rtmps://live.twitch.tv/app".into();
            e.key_saved = true;
            e.reconnect = false;
        }
        d.set_encoding(
            id,
            DestinationEncoding::Own {
                encoding: Encoding::new(4_500),
                output_size: Some(OutputSize {
                    width: 1280,
                    height: 720,
                }),
            },
        )
        .unwrap();
        let text = toml::to_string(&d).unwrap();
        assert!(!text.contains("key ="), "{text}");
        let back: Destinations = toml::from_str(&text).unwrap();
        assert_eq!(back, d);
        assert_eq!(Destinations::from_section(Ok(Some(back))), Ok(d));
    }

    #[test]
    fn a_newer_or_broken_section_reads_as_none_and_says_why() {
        let newer = Destinations {
            format_version: FORMAT_VERSION + 1,
            ..Destinations::default()
        };
        assert!(Destinations::from_section(Ok(Some(newer))).is_err());
        let mut twice = Destinations::default();
        twice.add(Canvas::Main, "a", CUSTOM).unwrap();
        let copy = twice.entries[0].clone();
        twice.entries.push(copy);
        assert!(Destinations::from_section(Ok(Some(twice))).is_err());
        let mut bad = Destinations::default();
        let id = bad.add(Canvas::Main, "a", CUSTOM).unwrap();
        bad.entry_mut(id).unwrap().server = "http://example.com".into();
        assert!(bad.problem().is_some());
        let mut swapped = Destinations::default();
        swapped
            .entries
            .push(Destination::new(VERTICAL_FIRST, Canvas::Main, "", CUSTOM));
        assert!(swapped.problem().is_some());
        assert!(Destinations::from_section(Err("unreadable".into())).is_err());
    }
}
