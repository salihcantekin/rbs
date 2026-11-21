//! Control API answers, from `ItemSetStyle` on (ADR-0159 decision 8; routed by `ApiTarget::run`).

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_item2(&mut self, command: rbs_api::Command) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::ItemSetStyle(p) => {
                let style = scene_style(&p.style).map_err(|why| refused(&why))?;
                // The canvas comes first in the error order (protocol 1.44).
                host.edit_canvas(p.canvas).map_err(|e| refused(&e))?;
                let item = ItemId::from_raw(p.item);
                let found = host
                    .collection
                    .scenes()
                    .flat_map(|s| s.items.iter())
                    .find(|i| i.id == item)
                    .map(|i| i.content);
                match found {
                    None => {
                        return Err(ApiError::new(
                            ErrorKind::NotFound,
                            format!("no item {}", p.item),
                        ));
                    }
                    Some(ItemContent::Scene(_)) => {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "shape and border are for source items, not nested scenes",
                        ));
                    }
                    Some(_) => {}
                }
                if p.canvas == OutputCanvas::Vertical
                    && host.collection.item(item).map(|(scene, _)| scene) == host.globals()
                {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "Global sources are not on the vertical canvas yet.",
                    ));
                }
                if host
                    .set_item_style(item, style, p.gesture, p.canvas)
                    .map_err(|e| failed(e.into_text()))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemSetScroll(p) => {
                let scroll = match &p.scroll {
                    Some(s) => Some(scene_scroll(s).map_err(|why| refused(&why))?),
                    None => None,
                };
                // The canvas comes first in the error order (protocol 1.44).
                host.edit_canvas(p.canvas).map_err(|e| refused(&e))?;
                let item = ItemId::from_raw(p.item);
                match host.collection.item(item).map(|(_, i)| i.content) {
                    None => {
                        return Err(ApiError::new(
                            ErrorKind::NotFound,
                            format!("no item {}", p.item),
                        ));
                    }
                    Some(ItemContent::Scene(_)) => {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "scroll is for source items, not nested scenes",
                        ));
                    }
                    Some(_) => {}
                }
                if p.canvas == OutputCanvas::Vertical
                    && host.collection.item(item).map(|(scene, _)| scene) == host.globals()
                {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "Global sources are not on the vertical canvas yet.",
                    ));
                }
                if host
                    .set_item_scroll(item, scroll, p.gesture, p.canvas)
                    .map_err(|e| failed(e.into_text()))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemScrollPause(p) => {
                host.api_scroll(p.item, p.canvas, ScrollControl::Pause, notify)
            }
            C::ItemScrollResume(p) => {
                host.api_scroll(p.item, p.canvas, ScrollControl::Resume, notify)
            }
            C::ItemScrollRestart(p) => {
                host.api_scroll(p.item, p.canvas, ScrollControl::Restart, notify)
            }
            C::ItemScrollSpeed(p) => {
                if !(MIN_SCROLL_SPEED..=MAX_SCROLL_SPEED).contains(&p.speed) {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "speed must be 1 to 4000; pause is item.scroll_pause",
                    ));
                }
                let speed = ScrollControl::Speed(f64::from(p.speed));
                host.api_scroll(p.item, p.canvas, speed, notify)
            }
            C::EditUndo | C::EditRedo => {
                let done = host.undo_or_redo(matches!(command, C::EditUndo));
                host.apply(notify);
                done.map_err(|(empty, why)| {
                    let kind = if empty {
                        ErrorKind::Conflict
                    } else {
                        ErrorKind::UndoConflict
                    };
                    ApiError::new(kind, why)
                })?;
                Ok(Response::Done(Empty {}))
            }
            C::OutputSetWithVertical(p) => {
                let flags = host.verticals.vertical_flags.set(p.kind, p.on);
                if flags != host.verticals.vertical_flags {
                    host.verticals.vertical_flags = flags;
                    notify(Update::VerticalFlags(flags));
                    host.settings_changed();
                }
                Ok(Response::Done(Empty {}))
            }
            C::OutputProfiles => Ok(Response::Profiles(ProfileList {
                profiles: vec![host.recording.profile.to_api_listing()],
            })),
            C::RecordStart(p) => {
                check_profile(&p.profile)?;
                let gate = start_gate(&p, host.recording.limit_policy)?;
                match p.canvas.unwrap_or_default().single() {
                    Some(one) => {
                        let c = host.api_canvas_of(Some(one))?;
                        let path = host.recording_file(c);
                        let iso = p.iso.unwrap_or(host.recording.separate_files);
                        self.record(c, path, false, iso, gate)
                    }
                    None => {
                        let iso = p.iso.unwrap_or(host.recording.separate_files);
                        self.start_all(api::OutputKind::Record, iso, gate)
                    }
                }
            }
            C::RecordEstimate(p) => {
                check_profile(&p.profile)?;
                let target = p.canvas.unwrap_or_default();
                let (c, all) = match target.single() {
                    Some(one) => (host.api_canvas_of(Some(one))?, false),
                    None => (OutputCanvas::Main, true),
                };
                host.record_estimate(c, all, p.iso)
                    .map(Response::RecordEstimate)
                    .map_err(|e| failed(e.into_text()))
            }
            C::RecordStartToFile(p) => {
                check_profile(&p.profile)?;
                let c = host.api_canvas_of(p.canvas)?;
                let path = PathBuf::from(&p.path);
                if !path.is_absolute() {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        format!("{} is not a full path", p.path),
                    ));
                }
                self.record(c, path, true, false, StartGate::Older)
            }
            // Routed elsewhere by `ApiTarget::run`.
            _ => Err(rbs_api::control::unsupported(&command)),
        }
    }
}
