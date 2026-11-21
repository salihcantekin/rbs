//! Control API answers, from `VcamStart` on (ADR-0159 decision 8; routed by `ApiTarget::run`).

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_vcam(&mut self, command: rbs_api::Command) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::VcamStart => {
                if host.camera.is_some() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the virtual camera already sends",
                    ));
                }
                if host.camera_factory.is_none() {
                    return Err(failed(
                        "the virtual camera is not available on this computer",
                    ));
                }
                let Some(setup) = &host.camera_setup else {
                    return Err(failed("the studio is still starting; try again"));
                };
                if setup.busy {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the virtual camera is being changed in Settings; try again",
                    ));
                }
                match &setup.state {
                    Some(InstallState::NotInstalled) => {
                        return Err(failed(
                            "the virtual camera is not added: add it in Settings, Virtual camera",
                        ));
                    }
                    Some(InstallState::NeedsUpdate { .. }) => {
                        return Err(failed(
                            "the virtual camera needs an update: add it again in Settings, Virtual camera",
                        ));
                    }
                    Some(InstallState::Unsupported { reason }) => {
                        return Err(failed(format!(
                            "the virtual camera is not available on this computer: {reason}"
                        )));
                    }
                    // Not read yet: starting says whether it is added.
                    _ => {}
                }
                let settings = VirtualCameraSettings::new(setup.width, setup.height, setup.fps);
                host.start_camera_asked(settings, notify, false)
                    .map_err(|e| failed(e.into_text()))?;
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::VcamStop => {
                if host.camera.is_none() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the virtual camera does not send",
                    ));
                }
                host.stop_camera(notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::CalibrationRun(p) => {
                check_profile(&p.profile)?;
                let Some(one) = p.canvas.unwrap_or_default().single() else {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "one canvas per run: main or vertical",
                    ));
                };
                let c = host.api_canvas_of(Some(one))?;
                if host.switch_pending.is_some() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the studio is restarting",
                    ));
                }
                host.run_calibration(c).map_err(|e| match e {
                    calibration::RunRefusal::Conflict(why) => {
                        ApiError::new(ErrorKind::Conflict, why)
                    }
                    calibration::RunRefusal::NotFound(why) => {
                        ApiError::new(ErrorKind::NotFound, why)
                    }
                    calibration::RunRefusal::Failed(why) => failed(why),
                })?;
                Ok(Response::Done(Empty {}))
            }
            C::CalibrationCancel => {
                host.cancel_calibration();
                Ok(Response::Done(Empty {}))
            }
            C::CalibrationStatus => Ok(Response::CalibrationStatus(api::Snapshot {
                seq: host.events.seq_map([Topic::CalibrationState]),
                data: host.api_calibration(),
            })),
            C::ProtectionStatus => Ok(Response::ProtectionStatus(api::Snapshot {
                seq: host.events.seq_map([Topic::ProtectionChanged]),
                data: host.api_protection(),
            })),
            C::ReplayStatus => Ok(Response::ReplayStatus(api::Snapshot {
                // The state as it is now, so the data and its `seq` match.
                seq: {
                    host.replay_api_changed();
                    host.events.seq_map([Topic::ReplayChanged])
                },
                data: host.api_replay(),
            })),
            C::ReplayArm => {
                if host.studio.replay_armed() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the replay buffer is already armed",
                    ));
                }
                host.arm_replay(notify).map_err(|e| failed(e.into_text()))?;
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::ReplayDisarm => {
                if !host.studio.replay_armed() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the replay buffer is not armed",
                    ));
                }
                host.disarm_replay(notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::ReplaySave(p) => {
                if let Some(seconds) = p.seconds
                    && !(1..=host.api_replay().length_secs).contains(&seconds)
                {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        format!(
                            "seconds must be 1 to {} (the buffer's length)",
                            host.api_replay().length_secs
                        ),
                    ));
                }
                if !host.studio.replay_armed() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the replay buffer is not armed",
                    ));
                }
                // The file is written on a worker: the answer says the clip was
                // requested, and `replay.changed` carries where it went.
                host.save_replay(p.seconds)
                    .map_err(|why| ApiError::new(ErrorKind::Conflict, why.into_text()))?;
                host.replay_changed(notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::SettingsGet => Ok(Response::Settings(api::Snapshot {
                seq: host.events.seq_map([Topic::SettingsChanged]),
                data: host.api_settings(),
            })),
            // Routed elsewhere by `ApiTarget::run`.
            _ => Err(rbs_api::control::unsupported(&command)),
        }
    }
}
