//! Control API answers, from `SourceRename` on (ADR-0159 decision 8; routed by `ApiTarget::run`).

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_source(
        &mut self,
        command: rbs_api::Command,
    ) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::SourceRename(p) => {
                let changed = host
                    .rename_source(SourceId::from_raw(p.source), &p.name)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetVolume(p) => {
                let changed = host
                    .set_source_audio(SourceId::from_raw(p.source), Some(p.volume), None)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetMuted(p) => {
                let changed = host
                    .set_source_audio(SourceId::from_raw(p.source), None, Some(p.muted))
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetMonitor(p) => {
                let changed = host
                    .set_source_monitor(
                        SourceId::from_raw(p.source),
                        monitor::scene_mode(p.monitor),
                    )
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::MonitorStatus => Ok(Response::MonitorStatus(api::Snapshot {
                seq: {
                    host.monitor_api_changed();
                    host.events.seq_map([Topic::MonitorChanged])
                },
                data: host.api_monitor(),
            })),
            C::MonitorDevices => Ok(Response::MonitorDevices(host.api_monitor_devices())),
            C::MonitorSetDevice(p) => {
                host.set_monitor_device(p.device).map_err(|e| refused(&e))?;
                Ok(Response::Done(Empty {}))
            }
            C::MonitorSetVolume(p) => {
                host.set_monitor_volume(p.volume).map_err(|e| refused(&e))?;
                Ok(Response::Done(Empty {}))
            }
            C::MonitorSetSolo(p) => {
                host.set_monitor_solo(&p.sources).map_err(|e| refused(&e))?;
                Ok(Response::Done(Empty {}))
            }
            C::LoudnessStatus => Ok(Response::LoudnessStatus(host.api_loudness())),
            C::LoudnessSetEnabled(p) => {
                let done = host.set_loudness_enabled(p.on).map_err(|e| refused(&e));
                host.publish(notify);
                done?;
                Ok(Response::Done(Empty {}))
            }
            C::LoudnessReset => {
                host.reset_loudness().map_err(|e| refused(&e))?;
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetMixerHidden(p) => {
                let changed = host
                    .set_source_mixer_hidden(SourceId::from_raw(p.source), p.hidden)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasAddVertical(p) => {
                use rbs_api::request::VerticalLayout as L;
                let (width, height) = (p.width.unwrap_or(1080), p.height.unwrap_or(1920));
                let fits = |v: u32| {
                    (VERTICAL_MIN_SIDE..=VERTICAL_MAX_SIDE).contains(&v) && v.is_multiple_of(2)
                };
                if !fits(width) || !fits(height) {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        format!(
                            "width and height must be even and from {VERTICAL_MIN_SIDE} to {VERTICAL_MAX_SIDE}"
                        ),
                    ));
                }
                let spec = VerticalCanvas {
                    size: Size::new(width, height),
                    follow: p.follow.unwrap_or(true),
                    layout: p.layout.map(|l| match l {
                        L::Fit => AutoLayout::Fit,
                        L::Fill => AutoLayout::Fill,
                        L::Stack => AutoLayout::Stack,
                    }),
                };
                let done = host.add_vertical(spec).map_err(|e| refused(&e))?;
                if done {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasAutoLayout(p) => {
                use rbs_api::request::VerticalLayout as L;
                if p.canvas != OutputCanvas::Vertical {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "canvas must be \"vertical\" (arranging the main canvas is not in this version)",
                    ));
                }
                let mode = match p.mode {
                    L::Fit => AutoLayout::Fit,
                    L::Fill => AutoLayout::Fill,
                    L::Stack => AutoLayout::Stack,
                };
                let items = host
                    .auto_layout_vertical(mode, p.scene.map(SceneId::from_raw))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::AutoLayoutDone(rbs_api::request::AutoLayoutDone {
                    items,
                }))
            }
            C::CanvasSetVerticalSize(p) => {
                let fits = |v: u32| {
                    (VERTICAL_MIN_SIDE..=VERTICAL_MAX_SIDE).contains(&v) && v.is_multiple_of(2)
                };
                if !fits(p.width) || !fits(p.height) {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        format!(
                            "width and height must be even and from {VERTICAL_MIN_SIDE} to {VERTICAL_MAX_SIDE}"
                        ),
                    ));
                }
                let done = host
                    .set_vertical_size(Size::new(p.width, p.height))
                    .map_err(|e| refused(&e))?;
                if done {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasRemoveVertical(_) => {
                if host.vertical.is_none() {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        "there is no vertical canvas",
                    ));
                }
                let done = host.remove_vertical().map_err(|e| refused(&e))?;
                if done {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            // Routed elsewhere by `ApiTarget::run`.
            _ => Err(rbs_api::control::unsupported(&command)),
        }
    }
}
