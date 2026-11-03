//! `srt://` URLs.

/// A parsed `srt://host:port` URL. The stream id is never part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SrtUrl {
    /// Host name or IP address (IPv6 without brackets).
    pub host: String,
    /// UDP port of the listener.
    pub port: u16,
}

/// Parses an SRT URL: `srt://host:port`, optionally with a trailing `/`. SRT has no
/// default port, so the port is required. User info, paths and queries are refused:
/// the stream id goes in the secret (so it is never logged) and the options that
/// tools put in the query (`latency`, `streamid`, `mode`, ...) are adapter options
/// or not supported.
pub(crate) fn parse(url: &str) -> Result<SrtUrl, String> {
    let (scheme, rest) = url.split_once("://").ok_or("not a URL")?;
    if !scheme.eq_ignore_ascii_case("srt") {
        return Err(format!("scheme {scheme:?} (expected srt)"));
    }
    if rest.contains('?') {
        return Err(
            "a query in an SRT URL (put the stream id in the secret and the latency in the \
             latency_ms adapter option)"
                .into(),
        );
    }
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.contains('/') {
        return Err("a path in an SRT URL (expected srt://host:port)".into());
    }
    if authority.contains('@') {
        return Err("user info in an SRT URL (put the stream id in the secret)".into());
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']').ok_or("unclosed IPv6 address")?;
        (host, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err("SRT URL without a host".into());
    }
    if host.contains(':') && !authority.starts_with('[') {
        return Err("an IPv6 address in an SRT URL must be in brackets".into());
    }
    let port = port
        .ok_or("SRT URL without a port (SRT has no default port)")?
        .parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or("bad port in the SRT URL")?;
    Ok(SrtUrl {
        host: host.to_owned(),
        port,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn parses_srt_urls() {
        let u = parse("srt://ingest.example.com:9000").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("ingest.example.com", 9000));
        let u = parse("SRT://127.0.0.1:4200/").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("127.0.0.1", 4200));
        let u = parse("srt://[::1]:9000").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 9000));
    }

    #[test]
    fn refuses_bad_urls() {
        for url in [
            "rtmp://host:9000",
            "srt://host",
            "srt://host:",
            "srt://host:0",
            "srt://host:x",
            "srt://:9000",
            "srt://host:9000?streamid=abc",
            "srt://host:9000/live",
            "srt://user:pass@host:9000",
            "srt://[::1:9000",
            "srt://[::1]9000",
            "srt://[::1]",
            "srt://fe80::1",
            "host:9000",
        ] {
            assert!(parse(url).is_err(), "{url}");
        }
    }
}
