//! Control API answers, from `SceneRemove` on (ADR-0159 decision 8; routed by `ApiTarget::run`).

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_scene(&mut self, command: rbs_api::Command) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::SceneRemove(p) => {
                let done = host.remove_scene(SceneId::from_raw(p.scene));
                // A switch made before a refusal still reaches the studio.
                host.apply(notify);
                done.map_err(|e| refused(&e))?;
                Ok(Response::Done(Empty {}))
            }
            C::SceneRename(p) => {
                let changed = host
                    .rename_scene(SceneId::from_raw(p.scene), &p.name)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneSetActive(p) => {
                let changed = host
                    .set_scene_active(SceneId::from_raw(p.scene), p.active)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneStep(p) => {
                let changed = host
                    .navigate_scene(Some(p.step), None)
                    .map_err(|e| refused(&e));
                host.apply(notify);
                changed?;
                Ok(Response::Done(Empty {}))
            }
            C::ScenePosition(p) => {
                let changed = host
                    .navigate_scene(None, Some(p.n as usize))
                    .map_err(|e| refused(&e));
                host.apply(notify);
                changed?;
                Ok(Response::Done(Empty {}))
            }
            C::SceneDuplicate(p) => host.api_scene_duplicate(p, notify),
            C::SourceDuplicate(p) => host.api_source_duplicate(p, notify),
            C::SourceReset(p) => host.api_source_reset(p, notify),
            C::SourceCopyAttributes(p) => host.api_copy_attributes(p, notify),
            C::SourceAttributesClipboard => Ok(host.api_attributes_clipboard()),
            C::SourcePasteAttributes(p) => host.api_paste_attributes(p, notify),
            C::SceneAudioGet(p) => {
                let id = SceneId::from_raw(p.scene);
                host.levels_scene(id).map_err(|e| refused(&e))?;
                let audio = host.collection.scene(id).and_then(|s| s.audio.as_ref());
                Ok(Response::SceneLevels(rbs_api::request::SceneLevels {
                    scene: p.scene,
                    own: audio.is_some(),
                    levels: audio
                        .map(|a| {
                            a.levels
                                .iter()
                                .map(|(source, l)| rbs_api::request::SceneLevelInfo {
                                    source: source.get(),
                                    volume: l.volume,
                                    muted: l.muted,
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                }))
            }
            C::SceneAudioSet(p) => {
                let (turned_own, changed) = host
                    .scene_level_set(
                        SceneId::from_raw(p.scene),
                        SourceId::from_raw(p.source),
                        p.volume,
                        p.muted,
                    )
                    .map_err(|e| refused(&e))?;
                if turned_own {
                    // An undo step: saved and on the history like any edit.
                    host.apply(notify);
                } else if changed {
                    host.publish(notify);
                }
                Ok(Response::SceneLevelSet(rbs_api::request::SceneLevelSet {
                    turned_own,
                }))
            }
            C::SceneAudioOwn(p) => {
                if host
                    .scene_levels_own(SceneId::from_raw(p.scene))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneAudioShared(p) => {
                if host
                    .scene_levels_shared(SceneId::from_raw(p.scene))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneAudioResetSource(p) => {
                if host
                    .scene_level_reset(SceneId::from_raw(p.scene), SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?
                {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneSources(p) => {
                let id = SceneId::from_raw(p.scene);
                if !host.listed(id) {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("no scene {}", p.scene),
                    ));
                }
                Ok(Response::SceneSources(SceneSourceList {
                    scene: p.scene,
                    sources: host.rows_of(id).iter().map(api_source).collect(),
                }))
            }
            C::CanvasSetPreview(p) => {
                let changed = match p.scene {
                    None => host.set_studio_mode(false),
                    Some(raw) => {
                        let id = SceneId::from_raw(raw);
                        if host.preview().is_none() {
                            if !host.listed(id) {
                                return Err(refused(&Refusal::NotFound(format!(
                                    "There is no scene {raw}."
                                ))));
                            }
                            host.set_studio_mode(true)
                                .and_then(|_| host.set_preview_scene(id))
                                .map(|_| true)
                        } else {
                            host.set_preview_scene(id)
                        }
                    }
                }
                .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasTransitionProgress(p) => {
                if !p.t.is_finite() || !(0.0..=1.0).contains(&p.t) {
                    return Err(refused(&Refusal::Invalid(
                        "t must be a number from 0 to 1.".to_owned(),
                    )));
                }
                if p.canvas == Some(OutputCanvas::Vertical) {
                    return Err(refused(&Refusal::Conflict(
                        "Only the main canvas has a T-bar.".to_owned(),
                    )));
                }
                if host.preview().is_none() {
                    return Err(refused(&Refusal::Conflict(STUDIO_MODE_OFF.to_owned())));
                }
                host.program.pending_progress = Some(p.t);
                Ok(Response::Done(Empty {}))
            }
            C::CanvasTransition(p) => {
                let once = transition_once(p).map_err(|e| refused(&e))?;
                let changed = host.transition().map_err(|e| refused(&e))?;
                if changed {
                    host.studio.set_transition_once(once);
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneSwitch(p) => {
                let once = transition_once(p.once()).map_err(|e| refused(&e))?;
                let changed = host
                    .switch_scene(SceneId::from_raw(p.scene))
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.studio.set_transition_once(once);
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            // Routed elsewhere by `ApiTarget::run`.
            _ => Err(rbs_api::control::unsupported(&command)),
        }
    }
}
