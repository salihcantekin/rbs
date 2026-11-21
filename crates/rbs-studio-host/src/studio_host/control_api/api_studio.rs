//! Control API answers, from `StudioGet` on (ADR-0159 decision 8; routed by `ApiTarget::run`).

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_studio(
        &mut self,
        command: rbs_api::Command,
    ) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::StudioGet => {
                host.note_encoders(notify);
                Ok(Response::Studio(api::Snapshot {
                    seq: host.events.seq_map([Topic::CanvasChanged]),
                    data: api::StudioInfo {
                        canvas: host.api_canvas(),
                        kinds: host.kinds.iter().filter_map(|k| api_kind(*k)).collect(),
                        encoders: host
                            .recording
                            .encoders
                            .iter()
                            .map(|e| api::EncoderInfo {
                                id: e.id.clone(),
                                name: e.name.clone(),
                                rate_controls: e.options.rate_controls.clone(),
                                h264_profiles: e.options.h264_profiles.clone(),
                                b_frames_max: e.options.b_frames_max,
                            })
                            .collect(),
                        cannot_record: host.recording.cannot_record.clone(),
                    },
                }))
            }
            C::SourceList => Ok(Response::Sources(api::Snapshot {
                seq: host.events.seq_map([Topic::SourceChanged]),
                data: host
                    .heard
                    .iter()
                    .map(|row| with_input_counters(&host.studio, api_source(row)))
                    .collect(),
            })),
            C::SourceTargets(p) => {
                let kind = app_kind(p.kind);
                Ok(Response::Targets(TargetList {
                    kind: p.kind,
                    targets: host
                        .targets(kind)
                        .into_iter()
                        .map(|t| api::CaptureTarget {
                            name: t.name,
                            adapter: t.adapter,
                            target: t.target,
                            is_default: t.is_default,
                            size: t
                                .size
                                .map(|(width, height)| api::Dimensions { width, height }),
                        })
                        .collect(),
                }))
            }
            C::SourceAdd(p) if p.stamp.is_some() || p.path.is_some() || p.url.is_some() => {
                self.add_made(p)
            }
            C::SourceAdd(p) if p.kind == api::SourceKind::Text => {
                // A text has no target (ADR-0129 decision 8).
                let text = p.text.as_deref().unwrap_or("Text");
                let id = if p.global {
                    if p.scene.is_some() {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "a global source is in every scene: leave out scene",
                        ));
                    }
                    host.add_text(p.name.as_deref(), text, true)
                        .map_err(|e| refused(&e))?
                } else {
                    host.placing_into(p.scene, |h| {
                        h.add_text(p.name.as_deref(), text, false)
                            .map_err(|e| refused(&e))
                    })?
                };
                host.apply(notify);
                Ok(Response::SourceAdded(SourceAdded { source: id.get() }))
            }
            C::SourceAdd(p) => {
                if p.text.is_some() {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "text is only for a text source",
                    ));
                }
                let kind = app_kind(p.kind);
                let Some(mut target) = host
                    .targets(kind)
                    .into_iter()
                    .find(|t| t.adapter == p.adapter && t.target == p.target)
                else {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("{} has no target {:?}", p.adapter, p.target),
                    ));
                };
                if let Some(name) = p.name.filter(|n| !n.trim().is_empty()) {
                    target.name = name;
                }
                let id = if p.global {
                    // A global source (ADR-0065): in every scene, so no scene is named.
                    if p.scene.is_some() {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "a global source is in every scene: leave out scene",
                        ));
                    }
                    host.add_global(kind, &target).map_err(|e| refused(&e))?
                } else {
                    host.placing_into(p.scene, |h| {
                        let id = h.add(kind, &target).map_err(|e| failed(e.into_text()))?;
                        if p.with_audio {
                            // One undo step for the picture and its sound; a refused
                            // companion takes the picture back out.
                            if let Err(e) = h.add_companion(id, true) {
                                let _ = h.delete(id);
                                return Err(refused(&e));
                            }
                        }
                        Ok(id)
                    })?
                };
                host.apply(notify);
                Ok(Response::SourceAdded(SourceAdded { source: id.get() }))
            }
            C::SourceSetCompanionAudio(p) => {
                let picture = SourceId::from_raw(p.source);
                if host.collection.source(picture).is_none() {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("no source {}", p.source),
                    ));
                }
                let has = host
                    .collection
                    .sources()
                    .any(|s| s.companion_of() == Some(picture));
                if p.enabled != has {
                    if p.enabled {
                        host.add_companion(picture, false)
                            .map_err(|e| refused(&e))?;
                    } else {
                        host.remove_companion(picture).map_err(|e| refused(&e))?;
                    }
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceDelete(p) => {
                let id = SourceId::from_raw(p.source);
                let Some(name) = host.collection.source(id).map(|s| s.name.clone()) else {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("no source {}", p.source),
                    ));
                };
                host.delete(id).map_err(|e| failed(e.into_text()))?;
                host.apply(notify);
                notify(Update::Deleted {
                    name,
                    from_scene: false,
                });
                Ok(Response::Done(Empty {}))
            }
            C::SceneList => Ok(Response::Scenes(api::Snapshot {
                seq: host.events.seq_map([Topic::SceneChanged]),
                data: scene_cache::answer(host),
            })),
            C::SceneAdd(p) => {
                let name = p.name.as_deref();
                let id = host.add_scene(name).map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::SceneAdded(SceneAdded { scene: id.get() }))
            }
            // Routed elsewhere by `ApiTarget::run`.
            _ => Err(rbs_api::control::unsupported(&command)),
        }
    }
}
