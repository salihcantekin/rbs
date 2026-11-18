//! Pure rules of the page automation the runner applies (ADR-0149): which loads are
//! errors to retry, when to retry, where the user's script may run, and the engine's
//! zoom level. No clock: callers pass times from their own (ADR-0032).

use std::time::Duration;

/// HTTP statuses that count as a failed load (ADR-0149 decision 6). 500, 503 and
/// every other answer are pages and are shown.
pub fn is_retry_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 502 | 504)
}

/// The wait before retry number `attempt` (1-based): 2, 4, 8, 16, 32, then every 60
/// seconds. A `Retry-After` of 1 to 600 seconds on a 429, 502 or 504 replaces it.
pub fn retry_delay(attempt: u32, retry_after_s: Option<u32>) -> Duration {
    if let Some(s) = retry_after_s.filter(|s| (1..=600).contains(s)) {
        return Duration::from_secs(u64::from(s));
    }
    let shift = attempt.clamp(1, 6);
    Duration::from_secs((1u64 << shift).min(60))
}

/// The origin of an `http` or `https` URL (`scheme://host[:port]`, lowercase), or for
/// a `file` URL its directory (`file:///dir/`). `None` for anything else.
pub fn origin(url: &str) -> Option<String> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("file://") {
        let slash = url.rfind('/')?;
        return Some(url[..=slash].to_owned());
    }
    let (scheme, rest) = lower.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit('@').next()?;
    if host.is_empty() {
        return None;
    }
    let default = if scheme == "https" { ":443" } else { ":80" };
    let host = host.strip_suffix(default).unwrap_or(host);
    Some(format!("{scheme}://{host}"))
}

/// True when the page now at `current` may run the script of a source saved with
/// `saved` (ADR-0149 decision 3): the same origin, or for a `file` page a document
/// under the saved file's directory.
pub fn script_may_run(saved: &str, current: &str) -> bool {
    let (Some(want), Some(now)) = (origin(saved), origin(current)) else {
        return false;
    };
    if want.starts_with("file://") {
        now.starts_with(&want)
    } else {
        want == now
    }
}

/// The engine's zoom level for `percent`: Chromium zooms by 1.2 per level.
pub fn zoom_level(percent: u16) -> f64 {
    (f64::from(percent.clamp(25, 500)) / 100.0).ln() / 1.2f64.ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_transient_failures_are_retried() {
        for s in [408, 429, 502, 504] {
            assert!(is_retry_status(s), "{s}");
        }
        for s in [200, 404, 500, 503] {
            assert!(!is_retry_status(s), "{s}");
        }
    }

    #[test]
    fn retries_back_off_to_a_minute_and_honour_retry_after() {
        let s: Vec<u64> = (1..=7).map(|a| retry_delay(a, None).as_secs()).collect();
        assert_eq!(s, [2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(retry_delay(3, Some(120)), Duration::from_secs(120));
        assert_eq!(retry_delay(3, Some(0)), Duration::from_secs(8));
        assert_eq!(retry_delay(3, Some(601)), Duration::from_secs(8));
    }

    #[test]
    fn the_script_stays_on_its_origin() {
        let saved = "https://Example.com/overlay/chat?x=1";
        assert!(script_may_run(saved, "https://example.com:443/other"));
        assert!(!script_may_run(saved, "https://www.example.com/overlay"));
        assert!(!script_may_run(saved, "http://example.com/overlay"));
        assert!(!script_may_run(saved, "https://accounts.example.org/login"));
        assert!(!script_may_run(saved, "https://evil.com/@example.com"));
        assert!(script_may_run(
            "file:///C:/o/score.html",
            "file:///C:/o/sub/part.html"
        ));
        assert!(!script_may_run(
            "file:///C:/o/score.html",
            "file:///C:/x.html"
        ));
        assert!(!script_may_run(saved, "about:blank"));
    }

    #[test]
    fn zoom_maps_to_chromium_levels() {
        assert!(zoom_level(100).abs() < 1e-9);
        assert!((zoom_level(120) - 1.0).abs() < 1e-9);
        assert!(zoom_level(50) < 0.0);
        assert_eq!(zoom_level(10), zoom_level(25));
    }
}
