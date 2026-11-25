//! `group.get` and `group.apply` (protocol 1.108, ADR-0184, ADR-0193): every check
//! runs on the control thread before any state changes, and a saved change is one
//! undo step; the live Scroll operations act on the studio's run and are not undoable.
//! Routed by `control_api/api_group.rs`.

use std::collections::BTreeSet;

use rbs_api::request::{
    ApplyGroup, GetGroup, GroupBounds, GroupMemberState, GroupOperation, GroupRenderFailure,
    GroupRenderFailureReason, GroupRenderRole, GroupRunState, GroupScroll as ApiGroupScroll,
    GroupState,
};
use rbs_api::types::OutputCanvas;
use rbs_record::{GroupFailure, GroupFailureReason, GroupOccurrence};
use rbs_scene::MAX_CANVAS_SIDE;
use rbs_scene::model::{GroupId, GroupScroll, show_for_is_valid};
use rbs_ui::group_transform;
use rbs_ui::layout::Rect;
use rbs_ui::transform::SourceBox;

use super::super::arrange::drawn_box;
use super::super::*;

/// The most pixels one group Scroll intermediate may have (ADR-0193 decision 4).
const MAX_GROUP_PIXELS: u64 = 8_388_608;

/// A member's current layout on the named canvas.
struct Member {
    id: ItemId,
    layout: Layout,
    own: Option<CanvasId>,
    picture: bool,
    locked: bool,
}

fn crop_refusal(e: CropRefusal) -> Refusal {
    match e {
        CropRefusal::NotFound(t) => Refusal::NotFound(t),
        CropRefusal::Invalid(t) => Refusal::Invalid(t),
        CropRefusal::Conflict(t) => Refusal::Conflict(t),
        CropRefusal::NoPicture | CropRefusal::Failed(_) => Refusal::Failed(e.text()),
    }
}

fn valid(r: Rect) -> Option<Rect> {
    [r.x, r.y, r.w, r.h]
        .iter()
        .all(|v| v.is_finite())
        .then_some(r)
        .filter(|r| r.w > 0.0 && r.h > 0.0)
}

fn bounds(r: Rect) -> GroupBounds {
    GroupBounds {
        x: r.x,
        y: r.y,
        width: r.w,
        height: r.h,
    }
}

fn scene_group_scroll(s: &ApiGroupScroll) -> Result<GroupScroll, Refusal> {
    let scroll = scene_scroll(&api::ItemScroll {
        direction: s.direction,
        speed: s.speed,
        gap: s.gap,
        repeat: s.repeat,
        start: s.start,
        fade: s.fade,
        ramp_ms: s.ramp_ms,
        on_return: api::ScrollReturn::Restart,
    })?;
    Ok(GroupScroll::from_scroll(scroll))
}

/// The studio's failures as `group.get` reports them; an occurrence id is its role
/// and its place among that role's failures.
pub(super) fn api_failures(failures: Vec<GroupFailure>) -> Vec<GroupRenderFailure> {
    let mut seen = BTreeMap::new();
    failures
        .into_iter()
        .map(|f| {
            let role = match f.occurrence {
                GroupOccurrence::Program => GroupRenderRole::Program,
                GroupOccurrence::Preview => GroupRenderRole::Preview,
                GroupOccurrence::Incoming => GroupRenderRole::Incoming,
                GroupOccurrence::Outgoing => GroupRenderRole::Outgoing,
                GroupOccurrence::Hold => GroupRenderRole::Hold,
            };
            let n = seen.entry(f.occurrence).or_insert(0u32);
            *n += 1;
            GroupRenderFailure {
                occurrence: format!("{:?}-{n}", f.occurrence).to_lowercase(),
                role,
                reason: match f.reason {
                    GroupFailureReason::InvalidBounds => GroupRenderFailureReason::InvalidBounds,
                    GroupFailureReason::TooLarge => GroupRenderFailureReason::TooLarge,
                    GroupFailureReason::Capped => GroupRenderFailureReason::Budget,
                    GroupFailureReason::Allocation => GroupRenderFailureReason::Allocation,
                    GroupFailureReason::Render => GroupRenderFailureReason::Render,
                },
            }
        })
        .collect()
}

fn api_group_scroll(s: GroupScroll) -> ApiGroupScroll {
    let a = api_scroll(&s.as_scroll());
    ApiGroupScroll {
        speed: a.speed,
        direction: a.direction,
        gap: a.gap,
        repeat: a.repeat,
        start: a.start,
        fade: a.fade,
        ramp_ms: a.ramp_ms,
    }
}

impl Host {
    /// The group's id and its members, bottom first. `NotFound` for an unknown scene
    /// or group.
    fn group_members(
        &self,
        scene: u32,
        group: u32,
    ) -> Result<(SceneId, GroupId, Vec<ItemId>), Refusal> {
        let scene = SceneId::from_raw(scene);
        self.group_scene(scene)?;
        let group = GroupId::from_raw(group);
        let s = self
            .collection
            .scene(scene)
            .ok_or_else(|| Refusal::NotFound(format!("There is no scene {}.", scene.get())))?;
        if !s.groups.iter().any(|g| g.id == group) {
            return Err(Refusal::NotFound(format!(
                "There is no group {}.",
                group.get()
            )));
        }
        let items = s
            .items
            .iter()
            .filter(|i| i.group == Some(group))
            .map(|i| i.id)
            .collect();
        Ok((scene, group, items))
    }

    fn members_on(&self, items: &[ItemId], on: OutputCanvas) -> Result<Vec<Member>, Refusal> {
        items
            .iter()
            .map(|&id| {
                let (_, item) = self
                    .collection
                    .item(id)
                    .ok_or_else(|| Refusal::NotFound(format!("There is no item {}.", id.get())))?;
                let (picture, locked) = (self.has_picture(item), item.locked);
                let (_, layout, own) = self.crop_target_on(id, on).map_err(crop_refusal)?;
                Ok(Member {
                    id,
                    layout,
                    own,
                    picture,
                    locked,
                })
            })
            .collect()
    }

    /// The position-and-size union of the picture members, without rotations.
    fn transform_box(members: &[Member]) -> Option<Rect> {
        let boxes: Vec<SourceBox> = members
            .iter()
            .filter(|m| m.picture)
            .map(|m| SourceBox::new(m.layout.position, m.layout.size))
            .collect();
        group_transform::union(&boxes).and_then(valid)
    }

    /// The rotated and flipped picture union: the group Scroll window.
    fn scroll_window(members: &[Member]) -> Option<Rect> {
        let boxes: Vec<SourceBox> = members
            .iter()
            .filter(|m| m.picture)
            .map(|m| drawn_box(m.layout.position, m.layout.size, m.layout.rotation))
            .collect();
        group_transform::union(&boxes).and_then(valid)
    }

    /// `group.get`: the edited scene's group state on one canvas, with its live run
    /// and render failures from the studio.
    pub(in crate::studio_host) fn group_get(&self, p: &GetGroup) -> Result<GroupState, Refusal> {
        let (scene, group, items) = self.group_members(p.scene, p.group)?;
        let canvas = self.edit_canvas(p.canvas)?;
        let found = self
            .collection
            .groups(scene)
            .map_err(|e| Refusal::of(&e))?
            .iter()
            .find(|g| g.id == group)
            .ok_or_else(|| Refusal::NotFound(format!("There is no group {}.", group.get())))?;
        let members = self.members_on(&items, p.canvas)?;
        let now = self.clock.now();
        let states = items
            .iter()
            .rev()
            .filter_map(|&id| self.collection.item(id))
            .map(|(_, item)| {
                let left = self
                    .studio
                    .cue_remaining_ms(item.id, now)
                    .filter(|ms| *ms > 0);
                GroupMemberState {
                    item: item.id.get(),
                    visible: item.visible,
                    locked: item.locked,
                    show_for_ms: item.show_for_ms,
                    cued: left.is_some(),
                    remaining_ms: left,
                    has_picture: self.has_picture(item),
                }
            })
            .collect();
        Ok(GroupState {
            scene: scene.get(),
            group: group.get(),
            name: found.name.clone(),
            items: items.iter().rev().map(|i| i.get()).collect(),
            members: states,
            canvas: p.canvas,
            bounds: Self::transform_box(&members).map(bounds),
            scroll_bounds: Self::scroll_window(&members).map(bounds),
            scroll: found.scrolls.get(&canvas).copied().map(api_group_scroll),
            run: self
                .studio
                .group_run(scene, group, canvas)
                .map(|r| GroupRunState {
                    id: r.id,
                    paused: r.paused,
                    speed: r.speed as f32,
                }),
            render_failures: api_failures(self.studio.group_render_failures(scene, group, canvas)),
        })
    }

    /// `group.apply`: one all-or-none group operation. `Ok(false)` when nothing
    /// changed; the caller reconciles the studio after `Ok(true)`.
    pub(in crate::studio_host) fn group_apply(&mut self, p: &ApplyGroup) -> Result<bool, Refusal> {
        if p.operation.is_live() && p.gesture != 0 {
            return Err(Refusal::Invalid(
                "A live group action takes no gesture.".into(),
            ));
        }
        if p.items.is_empty() {
            return Err(Refusal::Invalid("Name the group's members.".into()));
        }
        let wanted: BTreeSet<u32> = p.items.iter().copied().collect();
        if wanted.len() != p.items.len() {
            return Err(Refusal::Invalid("An item is named twice.".into()));
        }
        let (scene, group, items) = self.group_members(p.scene, p.group)?;
        if items.iter().map(|i| i.get()).collect::<BTreeSet<_>>() != wanted {
            return Err(Refusal::Conflict(
                "The group's members changed. Read the group again.".into(),
            ));
        }
        let canvas = self.edit_canvas(p.canvas)?;
        let name = self.group_name(scene, group)?;
        match &p.operation {
            GroupOperation::SetShowFor { show_for_ms } => {
                if !show_for_is_valid(*show_for_ms) {
                    return Err(Refusal::Invalid(
                        "A show duration is 1 to 600000 milliseconds.".into(),
                    ));
                }
                if self
                    .member_items(&items)
                    .all(|i| i.show_for_ms == *show_for_ms)
                {
                    return Ok(false);
                }
                let undo = self
                    .collection
                    .set_group_show_for(scene, group, &items, *show_for_ms)
                    .map_err(|e| Refusal::of(&e))?;
                self.edits
                    .history
                    .record(EditKind::ShowFor, &name, p.gesture, vec![undo]);
                Ok(true)
            }
            GroupOperation::Show {} => self.group_show(&items),
            GroupOperation::SetVisible { visible } => {
                self.group_flags(&items, &name, p.gesture, Some(*visible), None)
            }
            GroupOperation::SetLocked { locked } => {
                self.group_flags(&items, &name, p.gesture, None, Some(*locked))
            }
            GroupOperation::Move { .. }
            | GroupOperation::Resize { .. }
            | GroupOperation::Flip { .. } => {
                self.group_transform(scene, &items, &name, p.canvas, p.gesture, &p.operation)
            }
            GroupOperation::SetScroll { scroll } => {
                let Some((draft, undo)) =
                    self.group_scroll_draft(scene, group, &items, p.canvas, scroll.as_ref())?
                else {
                    return Ok(false);
                };
                self.commit_group_scroll(draft, &name, p.gesture, undo)
            }
            GroupOperation::RemoveMembers {} => {
                // The whole group with the companions and sources `item.remove` would
                // take, as one edit and undo step (ADR-0184).
                let mut draft = self.collection.clone();
                let (removed, undo) = draft
                    .remove_group_members(scene, group, &items)
                    .map_err(|e| Refusal::of(&e))?;
                self.collection = draft;
                let kind = if removed.source_deleted {
                    EditKind::Delete
                } else {
                    EditKind::Remove
                };
                self.edits
                    .history
                    .record(kind, &name, p.gesture, vec![undo]);
                Ok(true)
            }
            GroupOperation::ScrollSpeed { speed }
                if !(MIN_SCROLL_SPEED..=MAX_SCROLL_SPEED).contains(speed) =>
            {
                Err(Refusal::Invalid(SCROLL_ERROR.into()))
            }
            GroupOperation::ScrollPause {} => {
                self.group_scroll(scene, group, canvas, ScrollControl::Pause)
            }
            GroupOperation::ScrollResume {} => {
                self.group_scroll(scene, group, canvas, ScrollControl::Resume)
            }
            GroupOperation::ScrollRestart {} => {
                self.group_scroll(scene, group, canvas, ScrollControl::Restart)
            }
            GroupOperation::ScrollSpeed { speed } => self.group_scroll(
                scene,
                group,
                canvas,
                ScrollControl::Speed(f64::from(*speed)),
            ),
        }
    }

    /// A live call on the group's run (ADR-0185): not saved, not undoable. `Ok(true)`
    /// so the caller publishes the views.
    fn group_scroll(
        &mut self,
        scene: SceneId,
        group: GroupId,
        canvas: CanvasId,
        control: ScrollControl,
    ) -> Result<bool, Refusal> {
        match self
            .studio
            .group_scroll_control(scene, group, canvas, control)
        {
            Ok(()) => Ok(true),
            Err(ScrollRefused::NoScroll) => Err(Refusal::Invalid(
                "The group has no Scroll on this canvas.".into(),
            )),
            Err(ScrollRefused::NoRun) => Err(Refusal::Conflict(
                "The group's Scroll is not running.".into(),
            )),
        }
    }

    fn member_items<'a>(&'a self, items: &'a [ItemId]) -> impl Iterator<Item = &'a SceneItem> {
        items
            .iter()
            .filter_map(|&id| self.collection.item(id).map(|(_, item)| item))
    }

    /// Show (ADR-0193): every hidden member needs a duration, else nothing starts;
    /// then all their cues start or restart at one clock instant.
    fn group_show(&mut self, items: &[ItemId]) -> Result<bool, Refusal> {
        let mut cues = Vec::with_capacity(items.len());
        for item in self.member_items(items).filter(|i| !i.visible) {
            match item.show_for_ms {
                Some(ms) if show_for_is_valid(Some(ms)) => cues.push((item.id, ms)),
                _ => {
                    return Err(Refusal::Conflict(
                        "Every hidden member needs a show duration. Nothing was shown.".into(),
                    ));
                }
            }
        }
        Ok(!cues.is_empty() && self.studio.show_cues(&cues) > 0)
    }

    /// Sets the eye or the lock of every member as one undo step.
    fn group_flags(
        &mut self,
        items: &[ItemId],
        name: &str,
        gesture: u64,
        visible: Option<bool>,
        locked: Option<bool>,
    ) -> Result<bool, Refusal> {
        let mut draft = self.collection.clone();
        let mut undos = Vec::new();
        for item in self.member_items(items) {
            let undo = match (visible, locked) {
                (Some(v), _) if item.visible != v => draft.set_item_visible(item.id, v),
                (_, Some(l)) if item.locked != l => draft.set_item_locked(item.id, l),
                _ => continue,
            };
            undos.push(undo.map_err(|e| Refusal::of(&e))?);
        }
        if undos.is_empty() {
            return Ok(false);
        }
        self.collection = draft;
        let kind = if visible.is_some() {
            EditKind::Visibility
        } else {
            EditKind::Lock
        };
        self.edits.history.record(kind, name, gesture, undos);
        // Opening the eye drops running cues, as for one item (ADR-0105).
        if visible == Some(true) {
            for &item in items {
                self.studio.drop_cue(item);
            }
        }
        Ok(true)
    }

    /// Move, resize or flip every member on one canvas (ADR-0184). The box comes
    /// from the picture members; the mapping applies to every member's layout.
    fn group_transform(
        &mut self,
        scene: SceneId,
        items: &[ItemId],
        name: &str,
        on: OutputCanvas,
        gesture: u64,
        op: &GroupOperation,
    ) -> Result<bool, Refusal> {
        let shown = match on {
            OutputCanvas::Main => Some(self.edited()),
            OutputCanvas::Vertical => self.vertical_shown(),
        };
        if shown != Some(scene) {
            return Err(Refusal::Conflict(
                "This canvas edits another scene, so the group cannot be placed here.".into(),
            ));
        }
        let members = self.members_on(items, on)?;
        if members.iter().any(|m| m.locked) {
            return Err(Refusal::Conflict(
                "A member is locked. Unlock it to move the group.".into(),
            ));
        }
        let from = Self::transform_box(&members)
            .ok_or_else(|| Refusal::Conflict("The group has no picture to place.".into()))?;
        let (cx, cy) = (from.x + from.w / 2.0, from.y + from.h / 2.0);
        let map = |l: Layout| -> Result<Layout, Refusal> {
            let mut out = l;
            match *op {
                GroupOperation::Move { dx, dy } if dx.is_finite() && dy.is_finite() => {
                    out.position = [l.position[0] + dx, l.position[1] + dy];
                }
                GroupOperation::Resize {
                    x,
                    y,
                    width,
                    height,
                } if [x, y, width, height].iter().all(|v| v.is_finite())
                    && width > 0.0
                    && height > 0.0 =>
                {
                    let to = Rect {
                        x,
                        y,
                        w: width,
                        h: height,
                    };
                    let b =
                        group_transform::scaled(&[SourceBox::new(l.position, l.size)], from, to);
                    out.position = b[0].position;
                    out.size = b[0].size;
                }
                GroupOperation::Flip { axis } => {
                    out = match axis {
                        Axis::Horizontal => l.flipped_horizontally(),
                        Axis::Vertical => l.flipped_vertically(),
                    };
                    let c = out.centre();
                    let to = match axis {
                        Axis::Horizontal => [2.0 * cx - c[0], c[1]],
                        Axis::Vertical => [c[0], 2.0 * cy - c[1]],
                    };
                    out.position = [
                        out.position[0] + to[0] - c[0],
                        out.position[1] + to[1] - c[1],
                    ];
                }
                _ => {
                    return Err(Refusal::Invalid(
                        "A group box needs finite values and a positive size.".into(),
                    ));
                }
            }
            Ok(out)
        };
        let placed = members
            .iter()
            .map(|m| map(m.layout))
            .collect::<Result<Vec<_>, _>>()?;
        let mut undos = Vec::new();
        for (m, layout) in members.iter().zip(placed) {
            if layout == m.layout {
                continue;
            }
            match self.save_live_layout(m.id, m.own, layout) {
                Ok(u) => undos.push(u),
                Err(e) => {
                    // All or nothing: put back what was written.
                    while let Some(u) = undos.pop() {
                        if self.collection.undo(u).is_err() {
                            rbs_core::warn!(
                                rbs_core::log::Area::Studio,
                                "a group transform step could not be put back"
                            );
                        }
                    }
                    return Err(Refusal::of(&e));
                }
            }
        }
        if undos.is_empty() {
            return Ok(false);
        }
        let kind = if matches!(op, GroupOperation::Move { .. }) {
            EditKind::Place
        } else {
            EditKind::Transform
        };
        self.edits.history.record(kind, name, gesture, undos);
        Ok(true)
    }

    /// The collection with the group's Scroll set or cleared on one canvas, after
    /// the picture and window checks. `None` when it already has that setting.
    pub(super) fn group_scroll_draft(
        &self,
        scene: SceneId,
        group: GroupId,
        items: &[ItemId],
        on: OutputCanvas,
        scroll: Option<&ApiGroupScroll>,
    ) -> Result<Option<(SceneCollection, Undo)>, Refusal> {
        let canvas = self.edit_canvas(on)?;
        let settings = scroll.map(scene_group_scroll).transpose()?;
        let current = self
            .collection
            .groups(scene)
            .map_err(|e| Refusal::of(&e))?
            .iter()
            .find(|g| g.id == group)
            .and_then(|g| g.scrolls.get(&canvas).copied());
        if current == settings {
            return Ok(None);
        }
        if settings.is_some() {
            let members = self.members_on(items, on)?;
            if members.iter().any(|m| !m.picture) {
                return Err(Refusal::Conflict(
                    "A member has no picture, so the group cannot scroll.".into(),
                ));
            }
            let w = Self::scroll_window(&members)
                .ok_or_else(|| Refusal::Conflict("The group has no valid picture area.".into()))?;
            let side = |a: f32, b: f32| (b.ceil() - a.floor()) as u64;
            let (width, height) = (side(w.x, w.x + w.w), side(w.y, w.y + w.h));
            let max = u64::from(MAX_CANVAS_SIDE);
            if width > max || height > max || width * height > MAX_GROUP_PIXELS {
                return Err(Refusal::Conflict(format!(
                    "The group is too large to scroll: at most {MAX_GROUP_PIXELS} pixels and {max} on a side."
                )));
            }
        }
        let mut draft = self.collection.clone();
        let undo = draft
            .set_group_scroll(scene, group, items, canvas, settings)
            .map_err(|e| Refusal::of(&e))?;
        Ok(Some((draft, undo)))
    }

    /// Publishes a group Scroll draft as one undo step; the studio issues the new
    /// settings generation when it sees the change (ADR-0187).
    pub(super) fn commit_group_scroll(
        &mut self,
        draft: SceneCollection,
        name: &str,
        gesture: u64,
        undo: Undo,
    ) -> Result<bool, Refusal> {
        self.collection = draft;
        self.edits
            .history
            .record(EditKind::Scroll, name, gesture, vec![undo]);
        Ok(true)
    }
}
