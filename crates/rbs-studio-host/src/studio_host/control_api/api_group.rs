//! Control API answers for `group.*` (protocol 1.104, ADR-0182 decision 10; routed by
//! `ApiTarget::run`), and `group.get` and `group.apply` (protocol 1.111, ADR-0184,
//! ADR-0193).

use super::*;
use rbs_scene::model::GroupId;

impl ApiTarget<'_> {
    pub(super) fn answer_group(&mut self, command: rbs_api::Command) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::GroupCreate(p) => {
                let items: Vec<ItemId> = p.items.iter().map(|i| ItemId::from_raw(*i)).collect();
                let (id, name) = host
                    .group_items(SceneId::from_raw(p.scene), &items, p.name.as_deref())
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::GroupCreated(rbs_api::request::GroupCreated {
                    group: id.get(),
                    name,
                }))
            }
            C::GroupRemove(p) => {
                host.ungroup(SceneId::from_raw(p.scene), GroupId::from_raw(p.group))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::Done(Empty {}))
            }
            C::GroupRename(p) => {
                let changed = host
                    .rename_group(
                        SceneId::from_raw(p.scene),
                        GroupId::from_raw(p.group),
                        &p.name,
                    )
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::GroupList(p) => {
                let id = SceneId::from_raw(p.scene);
                if !host.listed(id) {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("no scene {}", p.scene),
                    ));
                }
                let scene = host.collection.scene(id).ok_or_else(|| {
                    ApiError::new(ErrorKind::NotFound, format!("no scene {}", p.scene))
                })?;
                // Members top first, as `scene.sources` lists them.
                let groups = scene
                    .groups
                    .iter()
                    .map(|g| rbs_api::request::GroupInfo {
                        id: g.id.get(),
                        name: g.name.clone(),
                        items: scene
                            .items
                            .iter()
                            .rev()
                            .filter(|i| i.group == Some(g.id))
                            .map(|i| i.id.get())
                            .collect(),
                    })
                    .collect();
                Ok(Response::Groups(rbs_api::request::GroupList {
                    scene: p.scene,
                    groups,
                }))
            }
            C::GroupGet(p) => Ok(Response::Group(
                host.group_get(&p).map_err(|e| refused(&e))?,
            )),
            C::GroupApply(p) => {
                if host.group_apply(&p).map_err(|e| refused(&e))? {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            _ => Err(rbs_api::control::unsupported(&command)),
        }
    }
}
