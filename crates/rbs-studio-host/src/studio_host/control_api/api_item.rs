//! Control API answers, from `ItemSetBox` on (ADR-0159 decision 8; routed by `ApiTarget::run`).

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_item(&mut self, command: rbs_api::Command) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::ItemSetBox(p) => {
                let source = SourceId::from_raw(p.source);
                let changed = match p.canvas {
                    OutputCanvas::Main => {
                        host.set_source_box(source, p.position, p.size, p.gesture)
                    }
                    OutputCanvas::Vertical => {
                        host.set_vertical_box(source, p.position, p.size, p.gesture)
                    }
                }
                .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::BoxPlaced(BoxPlaced {
                    seq: p.seq,
                    ok: true,
                }))
            }
            C::GlobalList => Ok(Response::GlobalSources(GlobalSourceList {
                sources: host.global_rows().iter().map(api_source).collect(),
            })),
            C::GlobalAdd(p) => {
                let item = host
                    .make_global(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::ItemAdded(ItemAdded { item: item.get() }))
            }
            C::GlobalRemove(p) => {
                let done = host
                    .unmake_global(SourceId::from_raw(p.source), p.keep)
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(match done {
                    Ok(item) => Response::ItemAdded(ItemAdded { item: item.get() }),
                    Err(removed) => Response::ItemRemoved(item_removed(&removed)),
                })
            }
            C::ItemAdd(p) => {
                let item = host.placing_into(p.scene, |h| {
                    h.add_existing(SourceId::from_raw(p.source))
                        .map_err(|e| refused(&e))
                })?;
                host.apply(notify);
                Ok(Response::ItemAdded(ItemAdded { item: item.get() }))
            }
            C::ItemSetVisible(p) => {
                let changed = host
                    .set_item_visible(ItemId::from_raw(p.item), p.visible)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemSetLocked(p) => {
                let changed = host
                    .set_item_locked(ItemId::from_raw(p.item), p.locked)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemSetShowFor(p) => {
                let changed = host
                    .set_item_show_for(ItemId::from_raw(p.item), p.show_for_ms)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemShow(p) => {
                let started = host
                    .show_item(ItemId::from_raw(p.item))
                    .map_err(|e| refused(&e))?;
                if started {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemFlip(p) => {
                let op = match p.axis {
                    Axis::Horizontal => TransformOp::FlipHorizontal,
                    Axis::Vertical => TransformOp::FlipVertical,
                };
                host.api_transform(p.item, op, p.gesture, p.canvas, notify)
            }
            C::ItemRotate(p) => {
                if p.degrees % 90 != 0 || p.degrees == 0 || p.degrees.abs() > 360 {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "degrees must be a non-zero multiple of 90 from -360 to 360",
                    ));
                }
                host.api_transform(
                    p.item,
                    TransformOp::Turn(p.degrees as f32),
                    p.gesture,
                    p.canvas,
                    notify,
                )
            }
            C::ItemFit(p) => {
                let mode = match p.mode {
                    FitKind::Fit => FitMode::Fit,
                    FitKind::Fill => FitMode::Fill,
                    FitKind::Stretch => FitMode::Stretch,
                };
                host.api_transform(p.item, TransformOp::Fit(mode), p.gesture, p.canvas, notify)
            }
            C::ItemAlign(p) => {
                let axis = match p.axis {
                    None => CenterAxis::Both,
                    Some(AlignAxis::Both) => CenterAxis::Both,
                    Some(AlignAxis::Horizontal) => CenterAxis::Horizontal,
                    Some(AlignAxis::Vertical) => CenterAxis::Vertical,
                };
                host.api_transform(
                    p.item,
                    TransformOp::Center(axis),
                    p.gesture,
                    p.canvas,
                    notify,
                )
            }
            C::ItemResetTransform(p) => {
                host.api_transform(p.item, TransformOp::Reset, p.gesture, p.canvas, notify)
            }
            C::ItemSetRotation(p) => {
                if !p.degrees.is_finite() || p.degrees.abs() > 360.0 {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "degrees is a number from -360 to 360",
                    ));
                }
                // -0 and 360 are stored as 0 (`Layout::angled` wraps).
                let op = TransformOp::SetAngle(p.degrees as f32 + 0.0);
                host.api_transform(p.item, op, p.gesture, p.canvas, notify)
            }
            C::ItemSetCrop(p) => {
                let [l, t, r, b] = p.crop;
                let item = ItemId::from_raw(p.item);
                match host.set_item_crop(
                    item,
                    Crop::new(l, t, r, b),
                    p.keep_scale,
                    p.gesture,
                    p.canvas,
                ) {
                    Ok(changed) => {
                        if changed {
                            host.apply(notify);
                        }
                        Ok(Response::Done(Empty {}))
                    }
                    Err(e) => Err(ApiError::new(
                        match e {
                            CropRefusal::NotFound(_) => ErrorKind::NotFound,
                            CropRefusal::Invalid(_) => ErrorKind::InvalidParams,
                            CropRefusal::Conflict(_) => ErrorKind::Conflict,
                            CropRefusal::NoPicture | CropRefusal::Failed(_) => ErrorKind::Failed,
                        },
                        e.text(),
                    )),
                }
            }
            // Routed elsewhere by `ApiTarget::run`.
            _ => Err(rbs_api::control::unsupported(&command)),
        }
    }
}
