//! HTTPS GETs with `ureq`: TLS through rustls (ring) and the OS trust roots, no
//! automatic redirects, a connect timeout and a per-read stall timeout.

use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use rbs_add_ons::{Fetch, FetchError, Response};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

/// Name resolution plus connection (TLS included).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// No byte for this long while waiting for headers or reading the body.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The production [`Fetch`].
pub struct SysFetch {
    agent: ureq::Agent,
}

impl std::fmt::Debug for SysFetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SysFetch")
    }
}

impl Default for SysFetch {
    fn default() -> Self {
        Self::new()
    }
}

impl SysFetch {
    /// With the spec's timeouts (15 s connect, 30 s stall).
    pub fn new() -> Self {
        Self::with_timeouts(CONNECT_TIMEOUT, STALL_TIMEOUT)
    }

    /// With other timeouts (tests).
    pub fn with_timeouts(connect: Duration, stall: Duration) -> Self {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::PlatformVerifier)
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .build();
        let config = ureq::Agent::config_builder()
            .tls_config(tls)
            // Redirects come back to rbs-add-ons, which checks each hop.
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_resolve(Some(connect))
            .timeout_connect(Some(connect))
            .timeout_recv_response(Some(stall))
            .timeout_per_read(Some(stall))
            .user_agent(concat!("RBS/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Fetch for SysFetch {
    fn get(&self, url: &str) -> Result<Response, FetchError> {
        let response = match self.agent.get(url).call() {
            Ok(r) => r,
            Err(ureq::Error::Timeout(t)) => {
                return Err(FetchError::Network(format!("timed out ({t})")));
            }
            Err(e) => return Err(FetchError::Network(e.to_string())),
        };
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| FetchError::Network(format!("HTTP {status} without a Location")))?;
            return Ok(Response::Redirect(absolute(url, location)));
        }
        if !(200..300).contains(&status) {
            return Err(FetchError::Network(format!("HTTP {status}")));
        }
        Ok(Response::Body(Box::new(StallReader(
            response.into_body().into_reader(),
        ))))
    }
}

/// Turns a timed-out read into [`FetchError::Stalled`].
struct StallReader<R>(R);

impl<R: Read> Read for StallReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf).map_err(|e| {
            let timed_out = e.kind() == io::ErrorKind::TimedOut
                || e.get_ref()
                    .and_then(|inner| inner.downcast_ref::<ureq::Error>())
                    .is_some_and(|u| matches!(u, ureq::Error::Timeout(_)));
            if timed_out {
                io::Error::other(FetchError::Stalled)
            } else {
                e
            }
        })
    }
}

/// Resolves a `Location` header against the request URL. `rbs-add-ons` then checks
/// the scheme and the host of the result.
fn absolute(base: &str, location: &str) -> String {
    if location.contains("://") {
        return location.to_owned();
    }
    let scheme_end = base.find("://").map_or(0, |i| i + 3);
    if let Some(rest) = location.strip_prefix("//") {
        return format!("{}{rest}", &base[..scheme_end]);
    }
    let origin_end = base[scheme_end..]
        .find('/')
        .map_or(base.len(), |i| scheme_end + i);
    if location.starts_with('/') {
        return format!("{}{location}", &base[..origin_end]);
    }
    let path = &base[..base
        .rfind('/')
        .filter(|i| *i >= origin_end)
        .unwrap_or(origin_end)];
    format!("{path}/{location}")
}

#[cfg(test)]
mod tests {
    use super::absolute;

    #[test]
    fn locations_resolve_against_the_request() {
        let base = "https://a.example/x/y?q";
        assert_eq!(absolute(base, "https://b.example/z"), "https://b.example/z");
        assert_eq!(absolute(base, "//c.example/z"), "https://c.example/z");
        assert_eq!(absolute(base, "/z"), "https://a.example/z");
        assert_eq!(
            absolute("https://a.example/x/y", "z"),
            "https://a.example/x/z"
        );
        assert_eq!(absolute("https://a.example", "z"), "https://a.example/z");
    }
}
