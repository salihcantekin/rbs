//! Add source's shape, image, media file and browser page, sent as `source.add`
//! (ADR-0164 step 3): the new source is selected once it is there.

use rbs_api::Command as C;
use rbs_api::request as rq;
use rbs_api::types::SourceKind as K;
use rbs_api::{ApiError, Response};

use rbs_app::studio_host::ApiReply;

use super::{Applied, StudioUi};

/// The ticket of Add source › Browser's call: one at a time, and far from the
/// counter's tickets (`api_call.rs`).
pub(super) const BROWSER_TICKET: u64 = u64::MAX - 1;

/// `source.add` of `kind` with no target: the shape, file or address says what.
fn made(kind: K) -> rq::AddSource {
    rq::AddSource {
        kind,
        adapter: String::new(),
        target: String::new(),
        name: None,
        scene: None,
        global: false,
        with_audio: false,
        text: None,
        stamp: None,
        path: None,
        url: None,
    }
}

impl StudioUi {
    fn adding_source(&mut self, reveal_shape: bool) {
        self.deleted.clear();
        self.adding = true;
        self.reveal_shape = reveal_shape;
    }

    /// Adds a colour shape (0 circle … 6 arrow) and opens Shape and border.
    pub fn add_stamp(&mut self, kind: i32) {
        use rq::Stamp as S;
        let stamp = match kind {
            0 => S::Circle,
            1 => S::Square,
            2 => S::Rectangle,
            3 => S::RoundedRectangle,
            4 => S::Ellipse,
            5 => S::Triangle,
            6 => S::Arrow,
            _ => return,
        };
        self.adding_source(true);
        self.call(C::SourceAdd(rq::AddSource {
            stamp: Some(stamp),
            ..made(K::Color)
        }));
    }

    /// Adds a PNG the user picked. The new source is selected.
    pub fn add_image_file(&mut self, path: &std::path::Path) {
        self.add_file(K::Image, path);
    }

    /// Adds a media file the user picked (ADR-0108). The scripted demo decoder
    /// ignores file bytes; any regular file path is enough for a Program test.
    pub fn add_media_file(&mut self, path: &std::path::Path) {
        self.add_file(K::Media, path);
    }

    fn add_file(&mut self, kind: K, path: &std::path::Path) {
        self.adding_source(false);
        self.call(C::SourceAdd(rq::AddSource {
            path: Some(path.display().to_string()),
            ..made(kind)
        }));
    }

    /// Adds a browser page (ADR-0109); an empty `url` adds one that waits for its
    /// address, and Properties opens on its Page tab. Without the engine add-on,
    /// Settings › Add-ons opens on it instead (ADR-0145 decision 8).
    pub fn add_browser_page(&mut self, url: &str) {
        self.adding_source(false);
        let call = C::SourceAdd(rq::AddSource {
            url: Some(url.to_owned()),
            ..made(K::Browser)
        });
        self.call_as(call, ApiReply::QuietTicket(BROWSER_TICKET));
    }

    /// The answer to [`Self::add_browser_page`]: a refusal is shown, except the missing
    /// engine, which opens its add-on.
    pub(super) fn browser_answer(&mut self, result: Result<Response, ApiError>, out: &mut Applied) {
        let Err(e) = result else { return };
        self.adding = false;
        let field = |k: &str| {
            e.data
                .as_ref()
                .and_then(|d| d.get(k)?.as_str().map(str::to_owned))
        };
        match field("addon") {
            Some(addon) if field("reason").as_deref() == Some("needs_addon") => {
                self.open_add_on = Some(addon);
            }
            _ => out.error = Some(e.message),
        }
    }
}
