//! `rtmp://` and `rtmps://` URLs.

/// Default RTMP port.
pub(crate) const DEFAULT_PORT: u16 = 1935;
/// Default RTMPS port (RTMP over TLS, as YouTube, Facebook, Kick and X publish it).
pub(crate) const DEFAULT_TLS_PORT: u16 = 443;

/// A parsed `rtmp[s]://host[:port]/app` URL. The stream key is never part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RtmpUrl {
    /// Host name or IP address (IPv6 without brackets).
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Application: the whole path without the leading and trailing `/`, query
    /// included (`live`, `live2`, `app/instance`, `app?token=...`).
    pub app: String,
    /// The `tcUrl` of the `connect` command: `rtmp[s]://authority/app`.
    pub tc_url: String,
    /// `rtmps`: the connection is TLS (ADR-0127 decision 10).
    pub tls: bool,
}

/// Parses an RTMP URL. Only the `rtmp` and `rtmps` schemes (any case); user info is
/// refused, because credentials in a URL would be logged (the key goes in the secret).
pub(crate) fn parse(url: &str) -> Result<RtmpUrl, String> {
    let (scheme, rest) = url.split_once("://").ok_or("not a URL")?;
    let tls = if scheme.eq_ignore_ascii_case("rtmp") {
        false
    } else if scheme.eq_ignore_ascii_case("rtmps") {
        true
    } else {
        return Err(format!("scheme {scheme:?} (expected rtmp or rtmps)"));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };
    if authority.contains('@') {
        return Err("user info in an RTMP URL (put the stream key in the secret)".into());
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']').ok_or("unclosed IPv6 address")?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or("text after the IPv6 address")?),
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err("RTMP URL without a host".into());
    }
    let port = match port {
        None if tls => DEFAULT_TLS_PORT,
        None => DEFAULT_PORT,
        Some(p) => p
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or("bad port in the RTMP URL")?,
    };
    let app = path.trim_end_matches('/');
    if app.is_empty() {
        return Err("RTMP URL without an application (rtmp://host/app)".into());
    }
    Ok(RtmpUrl {
        host: host.to_owned(),
        port,
        app: app.to_owned(),
        tc_url: format!("{}://{authority}/{app}", if tls { "rtmps" } else { "rtmp" }),
        tls,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn parses_rtmp_urls() {
        let u = parse("rtmp://a.rtmp.youtube.com/live2").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.app.as_str()),
            ("a.rtmp.youtube.com", 1935, "live2")
        );
        assert_eq!(u.tc_url, "rtmp://a.rtmp.youtube.com/live2");
        let u = parse("RTMP://127.0.0.1:19350/app/inst/").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.app.as_str()),
            ("127.0.0.1", 19350, "app/inst")
        );
        assert_eq!(u.tc_url, "rtmp://127.0.0.1:19350/app/inst");
        let u = parse("rtmp://[::1]:1936/live?token=x").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.app.as_str()),
            ("::1", 1936, "live?token=x")
        );
        let u = parse("rtmp://[::1]/live").unwrap();
        assert_eq!(u.port, 1935);
        assert!(!u.tls);
    }

    #[test]
    fn parses_rtmps_urls() {
        let u = parse("rtmps://live-api-s.facebook.com:443/rtmp/").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.app.as_str(), u.tls),
            ("live-api-s.facebook.com", 443, "rtmp", true)
        );
        assert_eq!(u.tc_url, "rtmps://live-api-s.facebook.com:443/rtmp");
        let u = parse("RTMPS://a.rtmps.youtube.com/live2").unwrap();
        assert_eq!((u.port, u.tls), (443, true));
        assert_eq!(u.tc_url, "rtmps://a.rtmps.youtube.com/live2");
    }

    #[test]
    fn refuses_bad_urls() {
        for url in [
            "rtmpt://host/app",
            "srt://host:9000",
            "rtmp://host",
            "rtmp://host/",
            "rtmp:///app",
            "rtmp://host:0/app",
            "rtmp://host:x/app",
            "rtmp://user:key@host/app",
            "rtmp://[::1/app",
            "rtmp://[::1]x/app",
            "host/app",
        ] {
            assert!(parse(url).is_err(), "{url}");
        }
    }
}
