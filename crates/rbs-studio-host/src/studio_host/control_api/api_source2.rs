//! Control API answers, from `SourceGetSettings` on (ADR-0159 decision 8; routed by `ApiTarget::run`).

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_source2(
        &mut self,
        command: rbs_api::Command,
    ) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        use rbs_api::request as rq;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::SourceGetSettings(p) => {
                let settings = host
                    .source_settings_json(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?;
                Ok(Response::SourceSettings(SourceSettingsInfo {
                    source: p.source,
                    settings,
                }))
            }
            C::SourceSetSettings(p) => {
                if host
                    .patch_source_settings_in(SourceId::from_raw(p.source), &p.settings, p.gesture)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetText(p) => {
                if host
                    .set_source_text(SourceId::from_raw(p.source), &p.text)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::TextTimer(p) => host.text_timer(p),
            C::TextFonts => match host.studio.text_fonts(&host.adapters.sources) {
                rbs_record::TextFonts::Ready(families) => {
                    Ok(Response::TextFonts(rbs_api::types::TextFonts {
                        families: families.as_ref().clone(),
                    }))
                }
                rbs_record::TextFonts::NotReady => {
                    // The window's picker gets the list once it is read (the control
                    // loop sends `Update::TextFonts`), as it always did.
                    host.texts.fonts_wanted = true;
                    Err(ApiError::new(
                        ErrorKind::NotReady,
                        "the font list is being read; ask again shortly",
                    ))
                }
                rbs_record::TextFonts::Unavailable => Err(ApiError::new(
                    ErrorKind::Failed,
                    "no text renderer is installed",
                )),
            },
            C::MediaPlay(rq::MediaSource { source })
            | C::MediaPause(rq::MediaSource { source })
            | C::MediaRestart(rq::MediaSource { source })
            | C::MediaSeek(rq::MediaSeek { source, .. }) => {
                let op = match command {
                    C::MediaPlay(_) => MediaOp::Play,
                    C::MediaPause(_) => MediaOp::Pause,
                    C::MediaSeek(p) => MediaOp::Seek(p.position_ms),
                    _ => MediaOp::Restart,
                };
                let done = host.media_command(source, op);
                // The next tick sends the media state, also when it looks unchanged.
                host.media_sent = None;
                done.map(Response::MediaState).map_err(|e| refused(&e))
            }
            C::MediaState(p) => host
                .media_command(p.source, MediaOp::State)
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
            C::BrowserReload(p) => host
                .browser_message(
                    p.source,
                    &rbs_sources::BrowserMessage::Reload {
                        bypass_cache: p.bypass_cache,
                    },
                )
                .map(|()| Response::Done(Empty {})),
            C::BrowserRunScript(p) => host
                .browser_message(p.source, &rbs_sources::BrowserMessage::RunScript(p.script))
                .map(|()| Response::Done(Empty {})),
            C::BrowserForgetSignIn(p) => {
                host.forget_sign_in(SourceId::from_raw(p.source), false)?;
                host.apply(notify);
                Ok(Response::Done(Empty {}))
            }
            C::BrowserInput(p) => host
                .browser_input(p.source, &p.events)
                .map(|()| Response::Done(Empty {})),
            C::SourceReload(p) => {
                match host
                    .reload_source(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?
                {
                    Reload::Retargeted => host.apply(notify),
                    // A restarted source's status is news (as the window had it).
                    Reload::Restarted(true) => host.publish(notify),
                    Reload::Restarted(false) => {}
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemRemove(p) => {
                let (name, removed) = host
                    .remove_from_scene(ItemId::from_raw(p.item))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                // The window's undo toast, whoever removed it (owner, 2026-11-21).
                notify(Update::Deleted {
                    name,
                    from_scene: !removed.source_deleted,
                });
                Ok(Response::ItemRemoved(item_removed(&removed)))
            }
            C::SceneMove(p) => match host.move_scene(SceneId::from_raw(p.scene), p.direction) {
                Ok(true) => {
                    host.apply(notify);
                    Ok(Response::Done(Empty {}))
                }
                Ok(false) => Ok(Response::Done(Empty {})),
                Err(e) => Err(ApiError::new(ErrorKind::Conflict, e.into_text())),
            },
            C::SourceSetActive(p) => {
                match host.set_source_active(SourceId::from_raw(p.source), p.active) {
                    Ok(true) => {
                        host.apply(notify);
                        Ok(Response::Done(Empty {}))
                    }
                    Ok(false) => Ok(Response::Done(Empty {})),
                    Err(e) => Err(ApiError::new(ErrorKind::NotFound, e.into_text())),
                }
            }
            C::LookList => Ok(Response::Looks(LookList {
                looks: rbs_scene::look::built_in()
                    .into_iter()
                    .map(|l| LookInfo {
                        id: l.id.to_owned(),
                        name: l.name.to_owned(),
                        group: api_look_group(l.group),
                    })
                    .collect(),
            })),
            C::LookApply(p) => {
                let changed = host
                    .apply_look(SourceId::from_raw(p.source), &p.look)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceAddSample(p) => {
                let id = host.add_sample(&p.sample).map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::SourceAdded(SourceAdded { source: id.get() }))
            }
            // Routed elsewhere by `ApiTarget::run`.
            _ => Err(rbs_api::control::unsupported(&command)),
        }
    }
}
