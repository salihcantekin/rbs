//! Control API answers, from `SourceGetSettings` on (ADR-0159 decision 8; routed by `ApiTarget::run`).

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_source2(
        &mut self,
        command: rbs_api::Command,
    ) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
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
                // A text path would put any readable file on air: the API cannot write
                // it until token scopes exist (ADR-0129 decision 8).
                if p.settings.contains_key(keys::FILE)
                    && host
                        .collection
                        .source(SourceId::from_raw(p.source))
                        .is_some_and(|s| SourceKind::of(&s.type_id) == Some(SourceKind::Text))
                {
                    return Err(ApiError::new(
                        ErrorKind::Forbidden,
                        "the control API cannot set a text source's file",
                    ));
                }
                if host
                    .patch_source_settings(SourceId::from_raw(p.source), &p.settings)
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
                rbs_record::TextFonts::NotReady => Err(ApiError::new(
                    ErrorKind::NotReady,
                    "the font list is being read; ask again shortly",
                )),
                rbs_record::TextFonts::Unavailable => Err(ApiError::new(
                    ErrorKind::Failed,
                    "no text renderer is installed",
                )),
            },
            C::MediaPlay(p) => host
                .media_command(p.source, MediaOp::Play)
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
            C::MediaPause(p) => host
                .media_command(p.source, MediaOp::Pause)
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
            C::MediaSeek(p) => host
                .media_command(p.source, MediaOp::Seek(p.position_ms))
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
            C::MediaRestart(p) => host
                .media_command(p.source, MediaOp::Restart)
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
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
                if host
                    .reload_source(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?
                    == Reload::Retargeted
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemRemove(p) => {
                let (_, removed) = host
                    .remove_from_scene(ItemId::from_raw(p.item))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::ItemRemoved(item_removed(&removed)))
            }
            C::SceneMove(p) => match host.move_scene(SceneId::from_raw(p.scene), p.direction) {
                Ok(true) => {
                    host.apply(notify);
                    Ok(Response::Done(Empty {}))
                }
                Ok(false) => Ok(Response::Done(Empty {})),
                Err(e) => Err(ApiError::new(ErrorKind::Conflict, e)),
            },
            C::SourceSetActive(p) => {
                match host.set_source_active(SourceId::from_raw(p.source), p.active) {
                    Ok(true) => {
                        host.apply(notify);
                        Ok(Response::Done(Empty {}))
                    }
                    Ok(false) => Ok(Response::Done(Empty {})),
                    Err(e) => Err(ApiError::new(ErrorKind::NotFound, e)),
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
