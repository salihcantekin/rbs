//! Control API answers, from `SettingsSet` on (ADR-0159 decision 8; routed by `ApiTarget::run`).

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_settings(
        &mut self,
        command: rbs_api::Command,
    ) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::SettingsSet(patch) => {
                let profile = if let Some(profiles) = &patch.profiles {
                    let [p] = profiles.as_slice() else {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "this server has exactly one recording profile",
                        ));
                    };
                    check_profile(&p.id)?;
                    let profile = RecordProfile::from_api_over(p, &host.recording.profile);
                    if let Some(why) = profile.problem() {
                        return Err(ApiError::new(ErrorKind::InvalidParams, why));
                    }
                    output_size_for_canvas(profile.output_size, host.main_canvas_size())?;
                    // A vertical recording without settings of its own runs on these.
                    let uses = host.outs[0].recording.is_some()
                        || (host.recording.vertical_profile.is_none()
                            && host.outs[slot(OutputCanvas::Vertical)].recording.is_some());
                    if uses && profile != host.recording.profile {
                        return Err(ApiError::new(
                            ErrorKind::Conflict,
                            "stop the recording before changing its settings",
                        ));
                    }
                    Some(profile)
                } else {
                    None
                };
                let stream = match &patch.stream {
                    Some(patch) => {
                        let Some(current) = &host.streaming.stream_profile else {
                            return Err(failed("the studio is still starting; try again"));
                        };
                        let stream = current.with_api(patch).map_err(|why| {
                            ApiError::new(ErrorKind::InvalidParams, why.into_text())
                        })?;
                        output_size_for_canvas(stream.output_size, host.main_canvas_size())?;
                        if host.outs[0].streaming.is_some() && stream != *current {
                            return Err(ApiError::new(
                                ErrorKind::Conflict,
                                "end the stream before changing its settings",
                            ));
                        }
                        Some(stream)
                    }
                    None => None,
                };
                if let Some(l) = &patch.loudness
                    && let Some(why) =
                        crate::loudness_settings::problem(l.target, l.true_peak_limit)
                {
                    return Err(ApiError::new(ErrorKind::InvalidParams, why));
                }
                let outputs = host.check_output_settings(&patch)?;
                // A canvas change is checked here; applying it is the part that can
                // still fail, and then nothing of the patch has been applied.
                let canvas = match &patch.canvas {
                    Some(canvas) => {
                        let c = canvas_profile(canvas)?;
                        let same = host
                            .collection
                            .canvas(host.canvas)
                            .is_some_and(|k| k.size == c.size() && k.frame_rate == c.frame_rate());
                        if same {
                            None
                        } else if host.any_recording() {
                            return Err(ApiError::new(
                                ErrorKind::Conflict,
                                "stop the recording before changing the canvas",
                            ));
                        } else if host.any_streaming() {
                            return Err(ApiError::new(
                                ErrorKind::Conflict,
                                "end the stream before changing the canvas",
                            ));
                        } else {
                            Some(c)
                        }
                    }
                    None => None,
                };
                // The loudness file is written before anything is applied (a failed
                // save changes nothing); a canvas change that then fails puts it back.
                let loudness = match &patch.loudness {
                    Some(l) => host
                        .save_loudness_settings(l.target, l.true_peak_limit)
                        .map_err(|e| refused(&e))?,
                    None => None,
                };
                if let Some(c) = canvas
                    && let Err(e) = host.change_canvas(c.size(), c.frame_rate(), notify)
                {
                    if loudness.is_some() {
                        host.undo_loudness_save();
                    }
                    return Err(failed(e.into_text()));
                }
                if let Some(profile) = profile
                    && profile != host.recording.profile
                {
                    host.recording.profile = profile.clone();
                    notify(Update::Profile(profile));
                    host.settings_changed();
                }
                if let Some(stream) = stream
                    && host.streaming.stream_profile.as_ref() != Some(&stream)
                {
                    host.streaming.stream_profile = Some(stream.clone());
                    notify(Update::StreamProfile(stream));
                    host.settings_changed();
                }
                if host.apply_output_settings(outputs, notify) {
                    host.settings_changed();
                }
                if let Some(saved) = loudness {
                    host.apply_loudness_settings(saved);
                    host.publish(notify);
                }
                Ok(Response::Settings(api::Snapshot {
                    seq: host.events.seq_map([Topic::SettingsChanged]),
                    data: host.api_settings(),
                }))
            }
            C::ProfileList => host.library_list(false),
            C::CollectionList => host.library_list(true),
            C::ProfileSwitch(r) => host.library_switch(false, r.id, notify),
            C::CollectionSwitch(r) => host.library_switch(true, r.id, notify),
            C::ImportObsScan => obs_import_api::scan(),
            C::ImportObsCheck(p) => obs_import_api::check(&p),
            C::ImportObsRun(p) => host.import_obs(&p, notify),
            C::AddonList => Ok(Response::AddOnList(host.addon_list())),
            C::AddonInstall(r) => host
                .addon_install(&r.id, notify)
                .map(|()| Response::Done(Empty {})),
            C::AddonCancel(r) => host.addon_cancel(&r.id).map(|()| Response::Done(Empty {})),
            C::AddonClean(r) => host
                .addon_clean(&r.id, notify)
                .map(|()| Response::Done(Empty {})),
            C::AddonRemove(r) => host
                .addon_remove(&r.id, notify)
                .map(|()| Response::Done(Empty {})),
            C::CollectionSetGlide(p) => {
                let easing = match p.easing {
                    api::GlideEasing::Linear => 0,
                    api::GlideEasing::EaseInOut => 1,
                };
                let changed = host
                    .set_glide(p.duration_ms, easing)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneSetTransition(p) => {
                let t = p.transition.map(|t| rbs_scene::SceneTransition {
                    kind: t.kind.map(scene_transition_kind),
                    duration_ms: t.duration_ms,
                });
                let changed = host
                    .set_scene_transition(SceneId::from_raw(p.scene), t)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            // Routed elsewhere by `ApiTarget::run`.
            _ => Err(rbs_api::control::unsupported(&command)),
        }
    }
}
