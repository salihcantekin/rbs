//! Control API answers for the additions of protocol 1.100: the voice chain, the
//! stinger file and `source.add` with a shape, a file or an address (ADR-0164
//! decision 6; routed by `ApiTarget::run`).

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_add(&mut self, command: rbs_api::Command) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::FilterAddVoiceChain(p) => {
                if host
                    .add_voice_chain(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CollectionSetStingerFile(p) => {
                let path = std::path::Path::new(&p.path);
                if path.is_relative() {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "The stinger file needs an absolute path.",
                    ));
                }
                let id = host.set_stinger_file(path).map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::SourceAdded(SourceAdded { source: id.get() }))
            }
            C::EditUndoDelete => {
                let delete = host
                    .edits
                    .history
                    .next_undo()
                    .is_some_and(|s| matches!(s.kind, EditKind::Delete | EditKind::Remove));
                if !delete {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "The newest step is not a delete or a remove from scene.",
                    ));
                }
                host.step(true, notify);
                Ok(Response::Done(Empty {}))
            }
            C::CanvasRehearse(p) => {
                if p.canvas == OutputCanvas::Vertical {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "The vertical canvas has no Preview picture.",
                    ));
                }
                host.rehearse()?;
                Ok(Response::Done(Empty {}))
            }
            C::AddonUpdate(r) => host
                .addon_update(&r.id, notify)
                .map(|()| Response::Done(Empty {})),
            // Routed elsewhere by `ApiTarget::run`.
            _ => Err(rbs_api::control::unsupported(&command)),
        }
    }

    /// `source.add` with `stamp`, `path` or `url`: a shape, an image or media file, or
    /// a browser page, placed as the window's Add source places it.
    pub(super) fn add_made(
        &mut self,
        p: rbs_api::request::AddSource,
    ) -> Result<Response, ApiError> {
        use api::SourceKind as K;
        use rbs_api::request::Stamp;
        let invalid = |text: &str| ApiError::new(ErrorKind::InvalidParams, text);
        let fits = match p.kind {
            K::Color => p.stamp.is_some() && p.path.is_none() && p.url.is_none(),
            K::Image | K::Media | K::Slideshow => {
                p.path.is_some() && p.stamp.is_none() && p.url.is_none()
            }
            K::Browser => p.url.is_some() && p.stamp.is_none() && p.path.is_none(),
            _ => false,
        };
        if !fits || p.text.is_some() {
            return Err(invalid(
                "stamp is for a color source, path for an image or media file or a slide show's folder, url for a browser page",
            ));
        }
        if p.global || p.with_audio {
            return Err(invalid("a shape, a file or a page is added to one scene"));
        }
        if p.path.is_some() && !(p.adapter.is_empty() && p.target.is_empty()) {
            return Err(invalid("give either a path or a target"));
        }
        if p.path
            .as_deref()
            .is_some_and(|f| std::path::Path::new(f).is_relative())
        {
            return Err(invalid("the file needs an absolute path"));
        }
        let notify = self.notify;
        let host = &mut *self.host;
        if p.kind == K::Browser && host.browser_needs_engine() {
            let mut data = serde_json::Map::new();
            data.insert("reason".into(), "needs_addon".into());
            data.insert("addon".into(), crate::add_ons::BROWSER_ENGINE.into());
            return Err(ApiError::with_data(
                ErrorKind::Failed,
                "The browser engine add-on is not installed.",
                data,
            ));
        }
        let name = p.name.as_deref().filter(|n| !n.trim().is_empty());
        let id = host.placing_into(p.scene, |h| match (p.stamp, p.path.as_deref()) {
            (Some(stamp), _) => {
                let kind = match stamp {
                    Stamp::Circle => 0,
                    Stamp::Square => 1,
                    Stamp::Rectangle => 2,
                    Stamp::RoundedRectangle => 3,
                    Stamp::Ellipse => 4,
                    Stamp::Triangle => 5,
                    Stamp::Arrow => 6,
                };
                h.add_stamp(kind, name).map_err(|e| refused(&e))
            }
            (None, Some(file)) if p.kind == K::Image => h
                .add_image_file(std::path::Path::new(file), name)
                // The decoder's words never name the path (ADR-0074 decision 9).
                .map_err(|e| refused(&e)),
            (None, Some(folder)) if p.kind == K::Slideshow => h
                .add_slideshow_folder(std::path::Path::new(folder), name)
                .map_err(|e| refused(&e)),
            (None, Some(file)) => h
                .add_media_file(std::path::Path::new(file), name)
                .map_err(|e| refused(&e)),
            (None, None) => h
                .add_browser_page(p.url.as_deref().unwrap_or_default(), name)
                .map_err(|e| refused(&e)),
        })?;
        host.apply(notify);
        Ok(Response::SourceAdded(SourceAdded { source: id.get() }))
    }
}

/// A `conflict` with `data.reason` (ADR-0164 decision 6).
fn conflict(reason: &str, text: &str) -> ApiError {
    let mut data = serde_json::Map::new();
    data.insert("reason".into(), reason.into());
    ApiError::with_data(ErrorKind::Conflict, text, data)
}

impl Host {
    /// `canvas.rehearse` (ADR-0135 decision 11): plays the transition on the Preview
    /// picture, or says why not.
    fn rehearse(&mut self) -> Result<(), ApiError> {
        if self.preview().is_none() {
            return Err(conflict("not_studio_mode", "Studio mode is off."));
        }
        if self.studio.rehearsing() {
            return Err(conflict("rehearsing", "A rehearsal is playing."));
        }
        if self.studio.transition_running(self.canvas) {
            return Err(conflict(
                "transition_running",
                "A transition is running or holding.",
            ));
        }
        if !self.studio.has_preview_picture() {
            return Err(ApiError::new(
                ErrorKind::Failed,
                "There is no Preview picture to rehearse on.",
            ));
        }
        if !self.studio.rehearsal_has_duration() {
            return Err(conflict(
                "no_duration",
                "A Cut or a transition of 0 s has nothing to rehearse.",
            ));
        }
        match self.studio.rehearse(&self.adapters.sources) {
            Ok(_) => Ok(()),
            Err(e) => Err(ApiError::new(
                ErrorKind::Failed,
                format!("The rehearsal could not start: {e}"),
            )),
        }
    }
}
