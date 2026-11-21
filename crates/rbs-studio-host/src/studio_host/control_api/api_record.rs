//! Control API answers, from `RecordStop` on (ADR-0159 decision 8; routed by `ApiTarget::run`).

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_record(
        &mut self,
        command: rbs_api::Command,
    ) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::RecordStop(p) => {
                let Some(one) = p.target().single() else {
                    return self.stop_all(api::OutputKind::Record);
                };
                let c = host.api_canvas_of(Some(one))?;
                if host.outs[slot(c)].recording.is_none() {
                    return Err(ApiError::new(ErrorKind::Conflict, "nothing records"));
                }
                host.stop_recording(c, None, notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::RecordPause(p) => self.pause_asked(p, Some(true)),
            C::RecordResume(p) => self.pause_asked(p, Some(false)),
            C::RecordPauseToggle(p) => self.pause_asked(p, None),
            C::StreamHold(p) => self.hold_asked(p.target(), Some(true), p),
            C::StreamResume(p) => self.hold_asked(
                p.target(),
                Some(false),
                rbs_api::request::StreamHold::default(),
            ),
            C::StreamHoldToggle(p) => self.hold_asked(p.target(), None, p),
            C::OutputBreakToggle => self.break_toggle(),
            C::MarkerAdd(p) => {
                let added = host.add_marker(&p)?;
                notify(host.markers_update(Some(&added)));
                Ok(Response::MarkersAdded(added))
            }
            C::MarkerRemove(p) => {
                host.remove_marker(p.canvas, p.index)?;
                notify(host.markers_update(None));
                Ok(Response::Done(Empty {}))
            }
            C::ProjectExport(p) => host
                .export_project(&p.recording, p.format)
                .map(Response::ProjectExported),
            C::RecordStatus(p) => {
                let c = host.api_canvas_of(p.canvas)?;
                let data = host.outs[slot(c)].record_state.clone();
                Ok(match c {
                    OutputCanvas::Main => Response::RecordStatus(api::Snapshot {
                        seq: host.events.seq_map([Topic::RecordState]),
                        data,
                    }),
                    OutputCanvas::Vertical => Response::RecordCanvasStatus(api::Snapshot {
                        seq: host.canvas_seq(Topic::RecordCanvasState, c),
                        data: api::CanvasRecordState {
                            canvas: c,
                            state: data,
                        },
                    }),
                })
            }
            C::StreamStart(p) => {
                let Some(one) = p.target().single() else {
                    return self.start_all(api::OutputKind::Stream, false, StartGate::Older);
                };
                let c = host.api_canvas_of(Some(one))?;
                self.stream(c)
            }
            C::StreamStop(p) => {
                let Some(one) = p.target().single() else {
                    return self.stop_all(api::OutputKind::Stream);
                };
                let c = host.api_canvas_of(Some(one))?;
                if host.outs[slot(c)].streaming.is_none() {
                    return Err(ApiError::new(ErrorKind::Conflict, "nothing streams"));
                }
                host.end_stream_asked(c, notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::StreamDestinations(p) => {
                let canvases = match p.canvas {
                    Some(c) => vec![host.api_canvas_of(Some(c))?],
                    None if host.verticals.vertical.is_some() => {
                        vec![OutputCanvas::Main, OutputCanvas::Vertical]
                    }
                    None => vec![OutputCanvas::Main],
                };
                Ok(Response::StreamDestinations(api::StreamDestinations {
                    destinations: canvases
                        .into_iter()
                        .flat_map(|c| host.api_destinations(c))
                        .collect(),
                }))
            }
            C::StreamDestinationStart(p) => self.destination_start(p.id),
            C::StreamDestinationStop(p) => self.destination_stop(p.id),
            C::StreamDestinationSetEnabled(p) => {
                if host.destination_canvas(p.id).is_none() {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("there is no destination {}", p.id),
                    ));
                }
                if host.streaming.destinations.set_enabled(p.id, p.enabled) {
                    notify(Update::StreamDestinations(
                        host.streaming.destinations.clone(),
                    ));
                }
                Ok(Response::Done(Empty {}))
            }
            C::StreamStatus(p) => {
                let c = host.api_canvas_of(p.canvas)?;
                let data = host.outs[slot(c)].stream_state.clone();
                Ok(match c {
                    OutputCanvas::Main => Response::StreamStatus(api::Snapshot {
                        seq: host.events.seq_map([Topic::StreamState]),
                        data,
                    }),
                    OutputCanvas::Vertical => Response::StreamCanvasStatus(api::Snapshot {
                        seq: host.canvas_seq(Topic::StreamCanvasState, c),
                        data: api::CanvasStreamState {
                            canvas: c,
                            state: data,
                        },
                    }),
                })
            }
            C::VcamStatus => Ok(Response::VcamStatus(api::Snapshot {
                // The state as it is now, so the data and its `seq` match.
                seq: {
                    host.camera_changed();
                    host.events.seq_map([Topic::VcamChanged])
                },
                data: host.api_camera(true),
            })),
            // Routed elsewhere by `ApiTarget::run`.
            _ => Err(rbs_api::control::unsupported(&command)),
        }
    }
}
