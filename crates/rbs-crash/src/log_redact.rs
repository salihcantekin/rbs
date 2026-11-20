//! Redaction of diagnostics log lines (ADR-0158 decision 6, spec `diagnostics-log.md`).
//!
//! Unlike the panic hook's [`redact`](crate::redact), this keeps a stream URL's host,
//! which support needs, and it waits for the secrets list instead of skipping it: it
//! runs on the log writer thread, never on a thread that must not wait.

use std::sync::atomic::Ordering;

use crate::{SECRET_SCHEMES, SECRETS, SECRETS_CHANGED, lower_find, replace_home};

/// Schemes whose query and fragment may hold a token.
const WEB_SCHEMES: [&str; 2] = ["http://", "https://"];

const REDACTED: &str = "<redacted>";

/// `text` ready for a log file: the home directory as `~`, a stream URL's user, path
/// and query replaced (scheme and host kept), a web URL's user, query and fragment
/// replaced, and every secret from [`add_secret`](crate::add_secret) replaced. Copies
/// the secrets list on each call; a writer that redacts many lines keeps a
/// [`LogRedactor`].
pub fn redact_for_log(text: &str, home: Option<&str>) -> String {
    LogRedactor::new(home.map(str::to_owned)).redact(text)
}

/// Redacts log lines with its own copy of the secrets list, taken again only when
/// [`add_secret`](crate::add_secret) changed it. The list's lock is held only for that
/// copy, never while a line is scanned, so the panic hook's non-waiting look at the
/// list is not turned away by a busy log writer.
#[derive(Debug)]
pub struct LogRedactor {
    home: Option<String>,
    secrets: Vec<String>,
    seen: Option<u64>,
}

impl LogRedactor {
    /// A redactor that shows `home` as `~`.
    pub fn new(home: Option<String>) -> LogRedactor {
        LogRedactor {
            home,
            secrets: Vec::new(),
            seen: None,
        }
    }

    /// `text` ready for a log file (see [`redact_for_log`]).
    pub fn redact(&mut self, text: &str) -> String {
        let mut out = match &self.home {
            Some(home) => replace_home(text, home, cfg!(windows)),
            None => text.to_owned(),
        };
        // Secrets first: a secret registered as a whole URL must match before the URL
        // is rewritten (a WHIP key sits in an http path, which is kept).
        self.refresh();
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTED);
            }
        }
        for scheme in SECRET_SCHEMES {
            out = rewrite_urls(&out, scheme, |rest| rest.find(['/', '?', '#']));
        }
        for scheme in WEB_SCHEMES {
            out = rewrite_urls(&out, scheme, |rest| rest.find(['?', '#']));
        }
        out
    }

    fn refresh(&mut self) {
        // Read the count before the copy: a change during the copy is seen next time.
        let now = SECRETS_CHANGED.load(Ordering::Acquire);
        if self.seen == Some(now) {
            return;
        }
        let list = SECRETS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.secrets.clone_from(&list);
        drop(list);
        self.seen = Some(now);
    }
}

/// Every URL with `scheme` in `text`, its user info replaced and everything from
/// `cut(rest)` (an index into the part after the host start) to the URL's end
/// replaced by its first character and `<redacted>`.
fn rewrite_urls(text: &str, scheme: &str, cut: impl Fn(&str) -> Option<usize>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = lower_find(rest, scheme) {
        let start = at + scheme.len();
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = url_end(tail);
        let url = &tail[..end];
        let host_end = url.find(['/', '?', '#']).unwrap_or(url.len());
        let (url, user) = match url[..host_end].rfind('@') {
            Some(i) => (&url[i + 1..], true),
            None => (url, false),
        };
        if user {
            out.push_str(REDACTED);
            out.push('@');
        }
        match cut(url) {
            Some(i) if i + 1 < url.len() => {
                out.push_str(&url[..=i]);
                out.push_str(REDACTED);
            }
            _ => out.push_str(url),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Where the URL at the start of `tail` ends: at white space or a quote, bracket or
/// angle bracket, but not inside a `<redacted>` an earlier step wrote.
fn url_end(tail: &str) -> usize {
    let mut i = 0;
    while i < tail.len() {
        if tail[i..].starts_with(REDACTED) {
            i += REDACTED.len();
            continue;
        }
        let Some(c) = tail[i..].chars().next() else {
            break;
        };
        if c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '>' | '<') {
            return i;
        }
        i += c.len_utf8();
    }
    tail.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_url_keeps_its_host() {
        assert_eq!(
            redact_for_log("to rtmp://live.twitch.tv/app/live_123 now", None),
            "to rtmp://live.twitch.tv/<redacted> now"
        );
        assert_eq!(
            redact_for_log("srt://h:9000?passphrase=abc", None),
            "srt://h:9000?<redacted>"
        );
        assert_eq!(redact_for_log("rtmp://host", None), "rtmp://host");
        assert_eq!(redact_for_log("rtmp://host/", None), "rtmp://host/");
        assert_eq!(
            redact_for_log("RTMPS://u:p@host/k", None),
            "RTMPS://<redacted>@host/<redacted>"
        );
    }

    #[test]
    fn a_web_url_loses_its_query_and_user() {
        assert_eq!(
            redact_for_log("page https://a.io/w/x?token=t#f.", None),
            "page https://a.io/w/x?<redacted>"
        );
        assert_eq!(
            redact_for_log("\"http://me:pw@a.io/p\"", None),
            "\"http://<redacted>@a.io/p\""
        );
        assert_eq!(redact_for_log("https://a.io/p", None), "https://a.io/p");
    }

    #[test]
    fn the_home_folder_and_secrets_are_replaced() {
        crate::add_secret("sk_log_test_9f8e");
        assert_eq!(
            redact_for_log("key sk_log_test_9f8e in /home/ann/x", Some("/home/ann")),
            "key <redacted> in ~/x"
        );
    }

    #[test]
    fn a_redactor_sees_secrets_added_after_it_was_made() {
        let mut r = LogRedactor::new(None);
        assert_eq!(r.redact("tok_later_5566"), "tok_later_5566");
        crate::add_secret("tok_later_5566");
        assert_eq!(r.redact("a tok_later_5566"), "a <redacted>");
    }

    #[test]
    fn a_secret_registered_as_a_whole_url_is_replaced_whole() {
        crate::add_secret("https://whip.example/whip/KEY_88bb?x=1");
        assert_eq!(
            redact_for_log("to https://whip.example/whip/KEY_88bb?x=1 now", None),
            "to <redacted> now"
        );
        crate::add_secret("KEY_99cc");
        assert_eq!(
            redact_for_log("https://whip.example/whip/KEY_99cc?x=1", None),
            "https://whip.example/whip/<redacted>?<redacted>"
        );
    }
}
