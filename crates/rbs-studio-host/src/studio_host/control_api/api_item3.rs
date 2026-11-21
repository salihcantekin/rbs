//! Control API answers for layer order and the item edits of protocol 1.100
//! (ADR-0164 decision 6; routed by `ApiTarget::run`).

use super::*;

/// Exactly one of `direction` and the `to_*` place, else `invalid_params`.
fn one_of<D, T>(direction: Option<D>, to: Option<T>) -> Result<Result<D, T>, ApiError> {
    match (direction, to) {
        (Some(d), None) => Ok(Ok(d)),
        (None, Some(t)) => Ok(Err(t)),
        _ => Err(ApiError::new(
            ErrorKind::InvalidParams,
            "Give either a direction or a place to move to.",
        )),
    }
}

/// `Done`, after publishing when something changed.
fn done(host: &mut Host, changed: bool, notify: &dyn Fn(Update)) -> Response {
    if changed {
        host.apply(notify);
    }
    Response::Done(Empty {})
}

impl ApiTarget<'_> {
    pub(super) fn answer_item3(&mut self, command: rbs_api::Command) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        let changed = match command {
            C::ItemMove(p) => match one_of(p.direction, p.to_item)? {
                Ok(direction) => {
                    let source = host
                        .collection
                        .item(ItemId::from_raw(p.item))
                        .and_then(|(_, it)| match it.content {
                            ItemContent::Source(id) => Some(id),
                            _ => None,
                        })
                        .ok_or_else(|| {
                            ApiError::new(
                                ErrorKind::NotFound,
                                "This source is not in the scene any more.",
                            )
                        })?;
                    host.move_source(source, direction)
                        .map_err(|e| ApiError::new(ErrorKind::Conflict, e.into_text()))?
                }
                Err(to) => host
                    .move_item_to(ItemId::from_raw(p.item), ItemId::from_raw(to))
                    .map_err(|e| refused(&e))?,
            },
            C::SceneMove(p) => {
                let scene = SceneId::from_raw(p.scene);
                match one_of(p.direction, p.to_scene)? {
                    Ok(direction) => host
                        .move_scene(scene, direction)
                        .map_err(|e| ApiError::new(ErrorKind::Conflict, e.into_text()))?,
                    Err(to) => host
                        .move_scene_to(scene, SceneId::from_raw(to))
                        .map_err(|e| refused(&e))?,
                }
            }
            C::ItemFitTextBox(p) => {
                let (source, _, _) = host
                    .crop_target_on(ItemId::from_raw(p.item), p.canvas)
                    .map_err(|e| refused(&e.into()))?;
                host.fit_text_box(source).map_err(|e| refused(&e))?
            }
            C::ItemSetFilled(p) => host
                .set_item_filled(ItemId::from_raw(p.item), p.on, p.canvas)
                .map_err(|e| refused(&e))?,
            C::ItemSetShownRegion(p) => {
                use rbs_api::request::ShownRegion as R;
                let region = match p.region {
                    R::Full => CropRegion::Full,
                    R::CanvasCentred => CropRegion::CanvasCentred,
                    R::LeftHalf => CropRegion::LeftHalf,
                    R::RightHalf => CropRegion::RightHalf,
                };
                host.set_shown_region(ItemId::from_raw(p.item), region, p.canvas)
                    .map_err(|e| refused(&e.into()))?
            }
            // Routed elsewhere by `ApiTarget::run`.
            _ => return Err(rbs_api::control::unsupported(&command)),
        };
        Ok(done(host, changed, notify))
    }
}
