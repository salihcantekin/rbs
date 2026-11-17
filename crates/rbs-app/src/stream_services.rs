//! Streaming service presets (ADR-0127 decision 12): a built-in list, data only.
//!
//! A preset fills a destination's server field, which stays editable, and its name.
//! Its limits only warn, before the stream starts; RBS never changes a setting by
//! itself. The engine never sees a service id (rule 4): presets live in the
//! composition root.

use std::sync::OnceLock;

use serde::Deserialize;

use crate::record_profile::Encoding;

/// One server a service offers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServiceServer {
    /// What it is ("RTMPS", "Automatic").
    pub label: String,
    /// Its URL; never holds a key.
    pub url: String,
}

/// What a service publishes for H.264/AAC ingest. Every limit is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Limits {
    /// The highest video bitrate it takes, in kbit/s.
    #[serde(default)]
    pub max_video_kbps: Option<u32>,
    /// The longest keyframe interval it takes, in seconds.
    #[serde(default)]
    pub max_keyframe_seconds: Option<u32>,
    /// When these were last compared with the service's guidance (YYYY-MM-DD).
    #[serde(default)]
    pub checked: String,
}

/// One preset.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Service {
    /// Its id, saved with a destination (`custom` for none).
    pub id: String,
    /// Its name, the destination's name by default.
    pub name: String,
    /// The servers it offers, the first one the default.
    #[serde(default)]
    pub servers: Vec<ServiceServer>,
    /// The ingest URL is per account: the server field stays empty, with the help.
    #[serde(default)]
    pub server_from_dashboard: bool,
    /// Where the user finds the key (and the server, for a per-account one).
    #[serde(default)]
    pub key_help: String,
    /// What it takes; empty when it publishes nothing RBS checks.
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Deserialize)]
struct File {
    service: Vec<Service>,
}

/// Every preset, in the order shown, Custom last.
pub fn all() -> &'static [Service] {
    static ALL: OnceLock<Vec<Service>> = OnceLock::new();
    ALL.get_or_init(|| {
        // The file is part of the build: a test parses it, so this never fails at run
        // time; an empty list (Custom only in effect) is the fallback all the same.
        toml::from_str::<File>(include_str!("stream_services.toml"))
            .map(|f| f.service)
            .unwrap_or_default()
    })
}

/// The preset `id`, if there is one.
pub fn get(id: &str) -> Option<&'static Service> {
    all().iter().find(|s| s.id == id)
}

/// The preset whose server list holds `url`, for a server typed or imported before
/// presets existed. `None` for any other server.
pub fn by_server(url: &str) -> Option<&'static Service> {
    let url = url.trim().trim_end_matches('/');
    all().iter().find(|s| {
        s.servers
            .iter()
            .any(|x| x.url.trim_end_matches('/').eq_ignore_ascii_case(url))
    })
}

/// What `encoding` breaks of `service`'s limits, in words ("Twitch: 8000 kbit/s is
/// above the 6000 kbit/s Twitch accepts."). Empty when nothing.
pub fn warnings(service: &Service, encoding: &Encoding) -> Vec<String> {
    let mut out = Vec::new();
    let l = &service.limits;
    if let Some(max) = l.max_video_kbps
        && encoding.video_kbps > max
    {
        out.push(format!(
            "{}: {} kbit/s is above the {max} kbit/s {} accepts.",
            service.name, encoding.video_kbps, service.name
        ));
    }
    if let Some(max) = l.max_keyframe_seconds
        && encoding.keyframe_seconds > max
    {
        out.push(format!(
            "{}: a keyframe every {} s is longer than the {max} s {} accepts.",
            service.name, encoding.keyframe_seconds, service.name
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_list_parses_with_custom_last_and_unique_ids() {
        let file: File = toml::from_str(include_str!("stream_services.toml")).unwrap();
        assert_eq!(file.service, all());
        let ids: Vec<_> = all().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["youtube", "twitch", "kick", "facebook", "x", "custom"]);
        for s in all() {
            assert!(!s.key_help.is_empty(), "{}", s.id);
            assert_eq!(s.server_from_dashboard, s.servers.is_empty() && s.id != "custom");
            for server in &s.servers {
                assert!(
                    crate::stream_profile::parse_server(&server.url).is_ok(),
                    "{}",
                    server.url
                );
            }
            let l = &s.limits;
            if l.max_video_kbps.is_some() || l.max_keyframe_seconds.is_some() {
                assert_eq!(l.checked.len(), 10, "{}", s.id);
            }
        }
    }

    #[test]
    fn limits_only_warn_and_name_the_service() {
        let twitch = get("twitch").unwrap();
        let mut e = Encoding::new(8000);
        assert_eq!(
            warnings(twitch, &e),
            ["Twitch: 8000 kbit/s is above the 6000 kbit/s Twitch accepts."]
        );
        e.video_kbps = 6000;
        assert!(warnings(twitch, &e).is_empty());
        assert!(warnings(get("custom").unwrap(), &Encoding::new(50_000)).is_empty());
    }

    #[test]
    fn a_known_server_finds_its_service() {
        assert_eq!(
            by_server("rtmp://live.twitch.tv/app/").map(|s| s.id.as_str()),
            Some("twitch")
        );
        assert!(by_server("rtmp://example.com/live").is_none());
    }
}
