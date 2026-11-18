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
        // A `..` segment (plain or escaped) could leave the directory.
        let lower = current.to_ascii_lowercase();
        let climbs = lower.contains("/../") || lower.contains("%2e");
        now.starts_with(&want) && !climbs
    } else {
        want == now
    }
}

/// The engine's zoom level for `percent`: Chromium zooms by 1.2 per level.
pub fn zoom_level(percent: u16) -> f64 {
    (f64::from(percent.clamp(25, 500)) / 100.0).ln() / 1.2f64.ln()
}

/// The one fixed script that applies custom CSS (ADR-0132 decision 4): a constructed
/// stylesheet added to `document.adoptedStyleSheets`, with the CSS only as an escaped
/// string value.
pub fn css_script(css: &str) -> String {
    format!(
        "(()=>{{const s=new CSSStyleSheet();s.replaceSync({});\
         document.adoptedStyleSheets=[...document.adoptedStyleSheets,s];}})();",
        js_string(css)
    )
}

/// The style sheet the runner adds to a page: with a transparent background, a rule
/// that clears the page's own background first (a page paints white unless it says
/// otherwise, so "Transparent background" alone would show nothing), then the
/// user's `css`, which can override it. Empty when there is nothing to add.
pub fn page_css(css: &str, transparent: bool) -> String {
    const CLEAR: &str = "html, body { background-color: rgba(0, 0, 0, 0) !important; }";
    match (transparent, css.is_empty()) {
        (false, _) => css.to_owned(),
        (true, true) => CLEAR.to_owned(),
        (true, false) => format!("{CLEAR}\n{css}"),
    }
}

/// `text` as a JavaScript string literal: nothing in it can end the literal.
pub fn js_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The directory of a `file` URL as a local path, percent escapes decoded; `None`
/// for any other URL. The runner watches it for `reload_on_file_change`.
pub fn file_dir(url: &str) -> Option<std::path::PathBuf> {
    let rest = url.trim().strip_prefix("file://")?;
    // `file:///C:/x` on Windows is `C:/x`; elsewhere `/x`.
    let path = if rest.len() > 3 && rest.as_bytes()[2] == b':' && rest.starts_with('/') {
        &rest[1..]
    } else {
        rest
    };
    let path = path.split(['?', '#']).next()?;
    let mut bytes = Vec::with_capacity(path.len());
    let raw = path.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%'
            && let Some(hex) = path.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            bytes.push(b);
            i += 3;
            continue;
        }
        bytes.push(raw[i]);
        i += 1;
    }
    let path = String::from_utf8(bytes).ok()?;
    let file = std::path::PathBuf::from(path);
    file.parent().map(std::path::Path::to_path_buf)
}

/// Bound of the hold after a first load's or a retry's first paint (ADR-0132
/// decision 4, ADR-0149 decision 6).
pub const HOLD_FIRST: Duration = Duration::from_millis(500);
/// Bound of the hold of a reload, from its start (ADR-0149 decision 2).
pub const HOLD_RELOAD: Duration = Duration::from_secs(2);
/// After the CSS or zoom was handed to the engine, frames painted within this time
/// may still be at the old style: they stay held.
pub const SETTLE: Duration = Duration::from_millis(50);

/// What the runner should do now, from [`Cycle::tick`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// Reload the main frame (`reload_every_s`).
    Reload,
    /// Load the failed page again (`retry_on_error`).
    Retry,
    /// Run the saved script (`script_every_s`).
    RunScript,
}

/// Why a main-frame navigation started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// The first load, or one the page started itself.
    Load,
    /// A reload: the timer, `browser.reload` or the user.
    Reload,
    /// A retry after a failed load.
    Retry,
}

/// The settings a [`Cycle`] follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CycleOptions {
    /// Seconds between reloads after a finished load; 0 for none.
    pub reload_every_s: u32,
    /// Seconds between script runs while loaded; 0 for none.
    pub script_every_s: u32,
    /// Run the script after each finished load.
    pub script_on_load: bool,
    /// Hold failed loads and retry them.
    pub retry_on_error: bool,
    /// The runner applies CSS or a zoom other than 100% at each commit, so frames
    /// right after it may be at the old style.
    pub restyles: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// A main-frame navigation runs; frames are held.
    Loading,
    /// The load failed; frames are held until the retry at `due`.
    Failed { due: Duration },
    /// Loaded (or released by its bound); timers run.
    Loaded,
}

/// The main frame's load cycle as the runner sees it (ADR-0132 decision 4, ADR-0149
/// decisions 2, 3 and 6): which frames are held, when to reload, retry and run the
/// script. Times are the runner's own clock since any fixed start; no clock is read
/// here (ADR-0032).
#[derive(Debug, Clone)]
pub struct Cycle {
    options: CycleOptions,
    phase: Phase,
    start: Start,
    started: Duration,
    /// The main frame's HTTP status, once its response arrived (0: none, as for
    /// `file`).
    status: Option<u16>,
    committed: Option<Duration>,
    first_paint: Option<Duration>,
    /// Frames may be sent.
    released: bool,
    attempts: u32,
    reload_due: Option<Duration>,
    script_due: Option<Duration>,
}

impl Cycle {
    /// A cycle before the first load.
    pub fn new(options: CycleOptions) -> Self {
        Self {
            options,
            phase: Phase::Loading,
            start: Start::Load,
            started: Duration::ZERO,
            status: None,
            committed: None,
            first_paint: None,
            released: false,
            attempts: 0,
            reload_due: None,
            script_due: None,
        }
    }

    /// A main-frame navigation starts at `now` (not a same-document one).
    pub fn navigate(&mut self, now: Duration, start: Start) {
        self.phase = Phase::Loading;
        self.start = start;
        self.started = now;
        self.status = None;
        self.committed = None;
        self.first_paint = None;
        self.released = false;
        self.reload_due = None;
        self.script_due = None;
    }

    /// The main frame's response arrived with `status`. A status of this decision's
    /// error list with `retry_on_error` fails the load: true then, and the runner
    /// stops it.
    pub fn response(&mut self, now: Duration, status: u16, retry_after_s: Option<u32>) -> bool {
        self.status = Some(status);
        if self.options.retry_on_error && is_retry_status(status) {
            self.fail(now, retry_after_s);
            return true;
        }
        false
    }

    /// The new document committed: the runner applies the zoom and CSS now.
    pub fn committed(&mut self, now: Duration) {
        if matches!(self.phase, Phase::Loading) {
            self.committed = Some(now);
        }
    }

    /// The main-frame load failed with a network error (`retry_after_s` only with a
    /// status). With `retry_on_error` off the engine's error page is shown.
    pub fn failed(&mut self, now: Duration, retry_after_s: Option<u32>) {
        if self.options.retry_on_error {
            self.fail(now, retry_after_s);
        } else {
            self.released = true;
        }
    }

    fn fail(&mut self, now: Duration, retry_after_s: Option<u32>) {
        self.attempts = self.attempts.saturating_add(1);
        let due = now + retry_delay(self.attempts, retry_after_s);
        self.phase = Phase::Failed { due };
        self.released = false;
        self.reload_due = None;
        self.script_due = None;
    }

    /// The main-frame load finished well. True when the script should run now
    /// (`script_on_load`; the runner still checks the origin).
    pub fn finished(&mut self, now: Duration) -> bool {
        if !matches!(self.phase, Phase::Loading) {
            return false;
        }
        self.phase = Phase::Loaded;
        self.attempts = 0;
        let after = |s: u32| (s > 0).then(|| now + Duration::from_secs(u64::from(s)));
        self.reload_due = after(self.options.reload_every_s);
        self.script_due = after(self.options.script_every_s);
        self.options.script_on_load
    }

    /// A frame was painted at `now`: true when it may be sent.
    pub fn painted(&mut self, now: Duration) -> bool {
        if self.released {
            return true;
        }
        if matches!(self.phase, Phase::Failed { .. }) {
            return false;
        }
        let Some(committed) = self.committed else {
            // The old document repaints while the new one loads: for a reload it
            // is the last good picture, for a page's own navigation it still shows.
            return self.start != Start::Retry;
        };
        let first = *self.first_paint.get_or_insert(now);
        let settled = !self.options.restyles || now >= committed + SETTLE;
        let status_ok = self.status.is_some();
        let bound = match self.start {
            Start::Reload => now >= self.started + HOLD_RELOAD,
            Start::Load | Start::Retry => now >= first + HOLD_FIRST,
        };
        self.released = (status_ok && settled) || bound;
        self.released
    }

    /// What is due at `now`, one at a time; the runner calls it from its loop.
    pub fn tick(&mut self, now: Duration) -> Option<Due> {
        match self.phase {
            Phase::Failed { due } if now >= due => {
                self.phase = Phase::Loading;
                Some(Due::Retry)
            }
            Phase::Loaded => {
                if self.reload_due.is_some_and(|due| now >= due) {
                    self.reload_due = None;
                    self.script_due = None;
                    return Some(Due::Reload);
                }
                let every = Duration::from_secs(u64::from(self.options.script_every_s));
                match self.script_due {
                    Some(due) if now >= due => {
                        self.script_due = Some(now + every);
                        Some(Due::RunScript)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Seconds until the retry while a failed load waits (`page_error`).
    pub fn retry_in(&self, now: Duration) -> Option<Duration> {
        match self.phase {
            Phase::Failed { due } => Some(due.saturating_sub(now)),
            _ => None,
        }
    }
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
        assert!(!script_may_run(
            "file:///C:/o/score.html",
            "file:///C:/o/../secret.html"
        ));
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn opts() -> CycleOptions {
        CycleOptions {
            retry_on_error: true,
            ..CycleOptions::default()
        }
    }

    #[test]
    fn a_first_load_holds_frames_until_the_status_and_style_are_in() {
        let mut c = Cycle::new(CycleOptions {
            restyles: true,
            ..opts()
        });
        c.navigate(ms(0), Start::Load);
        assert!(!c.response(ms(10), 200, None));
        c.committed(ms(20));
        assert!(!c.painted(ms(30)), "before the CSS settled");
        assert!(c.painted(ms(80)));
        assert!(c.painted(ms(81)), "released stays released");
    }

    #[test]
    fn a_failed_load_is_never_sent_and_retries_on_its_schedule() {
        let mut c = Cycle::new(opts());
        c.navigate(ms(0), Start::Load);
        assert!(c.response(ms(10), 502, None));
        assert!(!c.painted(ms(5_000)), "an error page is never sent");
        assert_eq!(c.retry_in(ms(1_010)), Some(ms(1_000)));
        assert_eq!(c.tick(ms(2_009)), None);
        assert_eq!(c.tick(ms(2_010)), Some(Due::Retry));
        c.navigate(ms(2_010), Start::Retry);
        c.failed(ms(2_100), None);
        assert_eq!(c.tick(ms(6_099)), None, "4 s after the second failure");
        assert_eq!(c.tick(ms(6_100)), Some(Due::Retry));
        c.navigate(ms(6_100), Start::Retry);
        c.response(ms(6_200), 429, Some(120));
        assert_eq!(c.retry_in(ms(6_200)), Some(Duration::from_secs(120)));
        // A good load resets the sequence.
        c.navigate(ms(200_000), Start::Retry);
        assert!(!c.response(ms(200_010), 503, None), "a 503 is a page");
        c.committed(ms(200_020));
        assert!(c.painted(ms(200_030)));
        c.finished(ms(200_040));
        c.navigate(ms(300_000), Start::Load);
        c.failed(ms(300_000), None);
        assert_eq!(c.retry_in(ms(300_000)), Some(Duration::from_secs(2)));
    }

    #[test]
    fn without_retry_the_error_page_shows() {
        let mut c = Cycle::new(CycleOptions::default());
        c.navigate(ms(0), Start::Load);
        assert!(!c.response(ms(10), 502, None));
        c.navigate(ms(100), Start::Load);
        c.failed(ms(110), None);
        assert!(c.painted(ms(120)));
        assert_eq!(c.tick(ms(100_000)), None);
    }

    #[test]
    fn a_reload_keeps_the_old_picture_up_to_its_bound() {
        let mut c = Cycle::new(CycleOptions {
            restyles: true,
            ..opts()
        });
        c.navigate(ms(0), Start::Reload);
        assert!(
            c.painted(ms(5)),
            "the old document is the last good picture"
        );
        c.committed(ms(10));
        assert!(!c.painted(ms(20)), "no status yet");
        assert!(c.painted(ms(2_000)), "the 2 s bound");
        let mut late = Cycle::new(opts());
        late.navigate(ms(0), Start::Load);
        late.committed(ms(10));
        assert!(!late.painted(ms(100)));
        assert!(late.painted(ms(600)), "500 ms after the first paint");
    }

    #[test]
    fn timers_run_from_the_end_of_a_load_and_pause_while_loading() {
        let mut c = Cycle::new(CycleOptions {
            reload_every_s: 60,
            script_every_s: 10,
            script_on_load: true,
            ..opts()
        });
        c.navigate(ms(0), Start::Load);
        assert!(c.finished(ms(1_000)), "script on load");
        assert_eq!(c.tick(ms(10_999)), None);
        assert_eq!(c.tick(ms(11_000)), Some(Due::RunScript));
        assert_eq!(c.tick(ms(11_001)), None);
        assert_eq!(c.tick(ms(21_000)), Some(Due::RunScript));
        assert_eq!(c.tick(ms(61_000)), Some(Due::Reload));
        c.navigate(ms(61_000), Start::Reload);
        assert_eq!(c.tick(ms(200_000)), None, "paused while loading");
        c.response(ms(61_100), 504, None);
        assert_eq!(c.tick(ms(62_000)), None, "paused while a retry waits");
        assert!(!c.finished(ms(62_000)), "a failed load does not finish");
    }

    #[test]
    fn css_is_only_ever_a_string_value() {
        let s = css_script("body{background:none}\"); alert(1); (\"\n\u{2028}</script>");
        assert!(s.starts_with("(()=>{const s=new CSSStyleSheet();s.replaceSync(\""));
        assert!(s.contains(r#"\"); alert(1); (\""#));
        assert!(s.contains(r"\n\u2028"));
        assert_eq!(js_string("a\\b\u{1}"), r#""a\\b\u0001""#);
    }

    #[test]
    fn a_transparent_page_clears_its_own_background_first() {
        assert_eq!(page_css("", false), "");
        assert_eq!(page_css("a{}", false), "a{}");
        assert!(page_css("", true).contains("background-color: rgba(0, 0, 0, 0) !important"));
        let both = page_css("body{background:red}", true);
        assert!(
            both.ends_with("\nbody{background:red}"),
            "the user's CSS comes last"
        );
    }

    #[test]
    fn a_file_page_names_its_directory() {
        assert_eq!(
            file_dir("file:///C:/o/a%20b/score.html"),
            Some(std::path::PathBuf::from("C:/o/a b"))
        );
        assert_eq!(
            file_dir("file:///home/u/o.html?x=1"),
            Some(std::path::PathBuf::from("/home/u"))
        );
        assert_eq!(file_dir("https://example.com/a.html"), None);
    }

    #[test]
    fn zoom_maps_to_chromium_levels() {
        assert!(zoom_level(100).abs() < 1e-9);
        assert!((zoom_level(120) - 1.0).abs() < 1e-9);
        assert!(zoom_level(50) < 0.0);
        assert_eq!(zoom_level(10), zoom_level(25));
    }
}
