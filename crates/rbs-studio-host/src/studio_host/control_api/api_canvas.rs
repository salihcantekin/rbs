//! Control API answers, from `CanvasSetMasterVolume` on (ADR-0159 decision 8; routed by `ApiTarget::run`).

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_canvas(
        &mut self,
        command: rbs_api::Command,
    ) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::CanvasSetMasterVolume(p) => {
                let changed = host
                    .set_master(Some(p.volume), None)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasSetMasterMuted(p) => {
                let changed = host
                    .set_master(None, Some(p.muted))
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::FilterTypes(p) => {
                let graph = &host.studio.settings().audio_graph;
                let setup = rbs_audio_filters::DspSetup::new(graph.format, graph.block_frames);
                let serves = host.adapters.sources.serves();
                Ok(Response::FilterTypes(filters::api_types(
                    p.domain, setup, serves,
                )))
            }
            C::FilterAdd(p) => {
                let settings = p.settings.unwrap_or_default();
                let id = host
                    .add_filter(SourceId::from_raw(p.source), &p.type_id, &settings)
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::FilterAdded(rbs_api::request::FilterAdded {
                    filter: id.get(),
                }))
            }
            C::FilterRemove(p) => {
                host.remove_filter(FilterId::from_raw(p.filter))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::Done(Empty {}))
            }
            C::FilterMove(p) => {
                if host
                    .move_filter(FilterId::from_raw(p.filter), p.index as usize)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::FilterSetEnabled(p) => {
                if host
                    .set_filter_enabled(FilterId::from_raw(p.filter), p.enabled)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::FilterSetSettings(p) => {
                if host
                    .patch_filter(FilterId::from_raw(p.filter), &p.settings)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetSyncOffset(p) => {
                if host
                    .set_source_sync_offset(SourceId::from_raw(p.source), p.offset_ms)
                    .map_err(|e| refused(&e))?
                {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SyncMeasureStart(p) => {
                let (measurement, reference) = host.start_sync_measure(
                    SourceId::from_raw(p.source),
                    p.reference.map(SourceId::from_raw),
                )?;
                Ok(Response::SyncMeasureStarted(
                    rbs_api::request::SyncMeasureStarted {
                        measurement,
                        reference: reference.get(),
                    },
                ))
            }
            C::SyncMeasureCancel(p) => {
                host.cancel_sync_measure(&p.measurement)?;
                Ok(Response::Done(Empty {}))
            }
            C::SyncDevices => Ok(Response::SyncDevices(rbs_api::request::SyncDeviceList {
                devices: host.api_sync_devices(),
            })),
            C::SyncForgetDevice(p) => {
                if host.forget_device(&sync_measure::file_key(p.key)) {
                    Ok(Response::Done(Empty {}))
                } else {
                    Err(ApiError::new(
                        ErrorKind::NotFound,
                        "No offset is remembered for this device.",
                    ))
                }
            }
            C::SyncApplyToDeviceSources(p) => {
                let source = SourceId::from_raw(p.source);
                let peers = host.device_peers(source);
                host.apply_to_device_sources(source)
                    .map_err(|e| refused(&e))?;
                if !peers.is_empty() {
                    host.publish(notify);
                }
                Ok(Response::SyncAppliedToDeviceSources(
                    rbs_api::request::SyncAppliedToDeviceSources {
                        changed: peers.iter().map(|(id, _)| id.get()).collect(),
                    },
                ))
            }
            C::SourceRetarget(p) => {
                let id = SourceId::from_raw(p.source);
                // For every caller the host's own list gives the name, the app and the
                // sound (ADR-0164 decision 6); a target it does not list is refused.
                let target = host.listed_target(id, &p.adapter, &p.target)?;
                if host.retarget_source(id, &target).map_err(|e| refused(&e))? {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SettingsRecentTargets(p) => {
                let kind = app_kind(p.kind);
                let targets = host
                    .recent_targets(kind)
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
                    .collect();
                Ok(Response::Targets(TargetList {
                    kind: p.kind,
                    targets,
                }))
            }
            C::SettingsClearRecentTargets(p) => {
                host.clear_recent_targets(app_kind(p.kind));
                Ok(Response::Done(Empty {}))
            }
            // Routed elsewhere by `ApiTarget::run`.
            _ => Err(rbs_api::control::unsupported(&command)),
        }
    }
}
