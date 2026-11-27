//! What Properties shows: one source, or a whole group selected in Sources (ADR-0184).
//! A group is read with `group.get` when the selection, its members' rows or the
//! canvas change, never per frame, and only while Properties shows (U5, U21): a
//! change while it is hidden is read once when it shows again; every group edit is one `group.apply` sent as a
//! control API command (ADR-0163). A refusal shows in the error bar and the group is
//! read again.

use std::cell::Cell;
use std::rc::Rc;

use rbs_api::request::{self as rq, GroupOperation, GroupState};
use rbs_api::types::{ScrollDirection, ScrollRepeat, ScrollStart};
use rbs_api::{ApiError, Command as C, Response};
use rbs_app::studio_host::{OutputCanvas, SourceRow};
use rbs_scene::model::Layout;
use slint::{Model as _, VecModel};

use super::{NEW_SCROLL_SPEED, StudioUi, ui_gesture};
use crate::ui::GroupPropsData;

/// A member row's part that `group.get` reads: a change asks again.
type MemberKey = (u32, bool, bool, u32, Option<Layout>, Option<Layout>, String);

/// The group Properties edits, as the Sources rows give it.
#[derive(Debug, Clone, PartialEq)]
struct Target {
    scene: u32,
    group: u32,
    /// Member item ids, as `group.apply` compares them.
    items: Vec<u32>,
    canvas: OutputCanvas,
    can_vertical: bool,
    /// The selected source the group was read for.
    anchor: i32,
    members: Vec<MemberKey>,
}

/// What Properties shows: the source it showed last, and the selected group with the
/// questions open about it.
pub(crate) struct PropsTarget {
    /// The source Properties last showed. It stays when the selection is cleared,
    /// so the panel is empty only when the scene has no sources.
    pub(super) inspected: Cell<i32>,
    /// None or one row: the window's `Studio.group-props`.
    model: Rc<VecModel<GroupPropsData>>,
    target: Option<Target>,
    asked: Option<u64>,
    applied: Vec<u64>,
    rev: i32,
    /// Properties is the shown tab of a window that is not hidden (U5, U21): only
    /// then is a group read; `stale` says a read waits for it to show.
    shown: bool,
    stale: bool,
}

impl Default for PropsTarget {
    fn default() -> Self {
        Self {
            inspected: Cell::new(-1),
            model: Rc::default(),
            target: None,
            asked: None,
            applied: Vec::new(),
            rev: 0,
            shown: false,
            stale: false,
        }
    }
}

impl PropsTarget {
    /// The model `Studio.group-props` shows.
    pub(crate) fn model(&self) -> slint::ModelRc<GroupPropsData> {
        slint::ModelRc::from(self.model.clone())
    }
}

/// The group `ids` (the selected sources) are exactly, if they are one whole group.
pub(super) fn whole_group(rows: &[SourceRow], ids: &[i32]) -> Option<u32> {
    let group = rows
        .iter()
        .find(|r| ids.first() == Some(&(r.id.get() as i32)))?
        .group?;
    let members = rows.iter().filter(|r| r.group == Some(group));
    let all = members.clone().all(|r| ids.contains(&(r.id.get() as i32)));
    (ids.len() > 1 && all && members.count() == ids.len()).then_some(group.get())
}

fn direction(i: i32) -> ScrollDirection {
    match i {
        1 => ScrollDirection::Right,
        2 => ScrollDirection::Up,
        3 => ScrollDirection::Down,
        _ => ScrollDirection::Left,
    }
}

/// What the window shows for `s`, read for `t`.
fn props_data(t: &Target, s: &GroupState, rev: i32) -> GroupPropsData {
    let m = &s.members;
    let b = s.bounds;
    let durations: Vec<Option<u32>> = m.iter().map(|m| m.show_for_ms).collect();
    let show_mixed = durations.windows(2).any(|w| w[0] != w[1]);
    let hidden = || m.iter().filter(|m| !m.visible);
    let scroll = s.scroll;
    let speed = scroll.map_or(NEW_SCROLL_SPEED, |s| s.speed);
    GroupPropsData {
        anchor: t.anchor,
        rev,
        name: s.name.as_str().into(),
        can_vertical: t.can_vertical,
        vertical: t.canvas == OutputCanvas::Vertical,
        x: b.map_or(0.0, |b| b.x),
        y: b.map_or(0.0, |b| b.y),
        w: b.map_or(0.0, |b| b.width),
        h: b.map_or(0.0, |b| b.height),
        box_why: if m.iter().any(|m| m.locked) {
            1
        } else {
            i32::from(b.is_none()) * 2
        },
        show_for_ms: durations
            .first()
            .copied()
            .flatten()
            .filter(|_| !show_mixed)
            .map_or(0, |ms| i32::try_from(ms).unwrap_or(i32::MAX)),
        show_mixed,
        show_why: if hidden().next().is_none() {
            1
        } else {
            i32::from(hidden().any(|m| m.show_for_ms.is_none())) * 2
        },
        scroll_on: scroll.is_some(),
        scroll_dir: scroll.map_or(0, |s| match s.direction {
            ScrollDirection::Right => 1,
            ScrollDirection::Up => 2,
            ScrollDirection::Down => 3,
            _ => 0,
        }),
        scroll_speed: speed,
        scroll_gap: scroll.map_or(0.0, |s| s.gap),
        scroll_repeat: scroll.map_or(0, |s| i32::from(s.repeat == ScrollRepeat::Once)),
        scroll_start: scroll.map_or(0, |s| i32::from(s.start == ScrollStart::Outside)),
        scroll_fade: scroll.map_or(0.0, |s| s.fade),
        scroll_why: if m.iter().any(|m| !m.has_picture) {
            1
        } else {
            i32::from(s.scroll_bounds.is_none()) * 2
        },
        run_paused: s.run.is_some_and(|r| r.paused),
        run_speed: s.run.map_or(speed, |r| r.speed),
    }
}

impl StudioUi {
    /// Which source Properties shows: the selection, else the last source it showed
    /// that still exists. `-1` otherwise.
    pub(super) fn remember_properties(&self) -> i32 {
        let ids: Vec<i32> = (0..self.sources.row_count())
            .filter_map(|i| self.sources.row_data(i).map(|s| s.id))
            .collect();
        let shown = properties_source(self.selected, self.props_target.inspected.get(), &ids);
        self.props_target.inspected.set(shown);
        shown
    }

    /// The group Properties edits, from `rows`: the selection when it is exactly one
    /// whole group of the scene being edited.
    fn group_target(&self, rows: &[SourceRow]) -> Option<Target> {
        let group = whole_group(rows, &self.picked.ids(self.selected))?;
        let scene = u32::try_from(self.scene_target(None)?).ok()?;
        let members: Vec<&SourceRow> = rows
            .iter()
            .filter(|r| r.group.map(|g| g.get()) == Some(group))
            .collect();
        let can_vertical = members.iter().all(|r| r.vertical_layout.is_some());
        let canvas = if can_vertical && self.props_on == OutputCanvas::Vertical {
            OutputCanvas::Vertical
        } else {
            OutputCanvas::Main
        };
        Some(Target {
            scene,
            group,
            items: members.iter().map(|r| r.item.get()).collect(),
            canvas,
            can_vertical,
            anchor: self.selected,
            members: members
                .iter()
                .map(|r| {
                    let (item, name) = (r.item.get(), r.group_name.clone());
                    let layouts = (r.layout, r.vertical_layout);
                    (
                        item,
                        r.visible,
                        r.locked,
                        r.show_for_ms,
                        layouts.0,
                        layouts.1,
                        name,
                    )
                })
                .collect(),
        })
    }

    /// Reads the selected group again when the selection, its members' rows or the
    /// canvas changed; forgets it when no whole group is selected.
    pub(super) fn read_group(&mut self, rows: &[SourceRow]) {
        let target = self.group_target(rows);
        self.retarget(target);
    }

    fn retarget(&mut self, target: Option<Target>) {
        if target == self.props_target.target {
            return;
        }
        self.props_target.target = target;
        if self.props_target.shown || self.props_target.target.is_none() {
            self.ask_group();
        } else {
            self.props_target.stale = true;
        }
    }

    /// Properties shows or hides; a group read that waited is made once it shows.
    pub(crate) fn set_properties_shown(&mut self, shown: bool) {
        let p = &mut self.props_target;
        p.shown = shown;
        if shown && std::mem::take(&mut p.stale) {
            self.ask_group();
        }
    }

    /// As [`StudioUi::read_group`], with the rows the window holds.
    pub(super) fn read_group_now(&mut self) {
        let rows = std::mem::take(&mut self.rows);
        self.read_group(&rows);
        self.rows = rows;
    }

    fn ask_group(&mut self) {
        let Some(t) = &self.props_target.target else {
            self.props_target.asked = None;
            self.props_target.model.set_vec(Vec::new());
            return;
        };
        let call = C::GroupGet(rq::GetGroup {
            scene: t.scene,
            group: t.group,
            canvas: t.canvas,
        });
        self.props_target.asked = Some(self.ask_quietly(call));
    }

    /// The answer to a group question or edit; the result back when the ticket is not
    /// one of them.
    pub(super) fn group_answer(
        &mut self,
        ticket: u64,
        result: Result<Response, ApiError>,
    ) -> Option<Result<Response, ApiError>> {
        let p = &mut self.props_target;
        if let Some(i) = p.applied.iter().position(|t| *t == ticket) {
            // Saved or refused, the group may differ from what is shown: read it again.
            p.applied.swap_remove(i);
            if p.shown {
                self.ask_group();
            } else {
                p.stale = true;
            }
            return None;
        }
        if p.asked != Some(ticket) {
            return Some(result);
        }
        p.asked = None;
        let (Some(t), Ok(Response::Group(s))) = (&p.target, result) else {
            // Gone meanwhile: the next row update forgets it.
            p.model.set_vec(Vec::new());
            return None;
        };
        if s.items.len() != t.items.len() {
            return None;
        }
        p.rev = p.rev.wrapping_add(1);
        let data = props_data(t, &s, p.rev);
        if p.model.row_count() == 1 {
            p.model.set_row_data(0, data);
        } else {
            p.model.set_vec(vec![data]);
        }
        None
    }

    /// Sends one `group.apply` for the selected group.
    fn group_apply(&mut self, operation: GroupOperation, gesture: u64) {
        let Some(t) = &self.props_target.target else {
            return;
        };
        let call = C::GroupApply(rq::ApplyGroup {
            scene: t.scene,
            group: t.group,
            items: t.items.clone(),
            canvas: t.canvas,
            operation,
            gesture,
        });
        self.deleted.clear();
        let ticket = self.ask(call);
        self.props_target.applied.push(ticket);
    }

    /// Timed show duration of every member; 0 clears it.
    pub fn group_set_show_for(&mut self, ms: i32) {
        let show_for_ms = u32::try_from(ms).ok().filter(|ms| *ms > 0);
        self.group_apply(GroupOperation::SetShowFor { show_for_ms }, 0);
    }

    /// Show now: every hidden member for its own duration (live).
    pub fn group_show(&mut self) {
        self.group_apply(GroupOperation::Show {}, 0);
    }

    /// Maps the covering box to a new one.
    pub fn group_set_box(&mut self, x: f32, y: f32, width: f32, height: f32, gesture: i32) {
        let op = GroupOperation::Resize {
            x,
            y,
            width,
            height,
        };
        self.group_apply(op, ui_gesture(gesture));
    }

    /// Sets or clears the group's own Scroll on Properties' canvas.
    #[allow(clippy::too_many_arguments)] // One value per Scroll field, as the window sends them.
    pub fn group_set_scroll(
        &mut self,
        on: bool,
        dir: i32,
        speed: f32,
        gap: f32,
        repeat: i32,
        start: i32,
        fade: f32,
        gesture: i32,
    ) {
        let scroll = on.then(|| rq::GroupScroll {
            speed,
            direction: direction(dir),
            gap,
            repeat: if repeat == 1 {
                ScrollRepeat::Once
            } else {
                ScrollRepeat::Loop
            },
            start: if start == 1 {
                ScrollStart::Outside
            } else {
                ScrollStart::Inside
            },
            fade,
            ramp_ms: rbs_scene::model::DEFAULT_SCROLL_RAMP_MS,
        });
        self.group_apply(GroupOperation::SetScroll { scroll }, ui_gesture(gesture));
    }

    /// Live, not undone: 0 pause, 1 resume, 2 restart, 3 speed.
    pub fn group_scroll_live(&mut self, action: i32, speed: f32) {
        let op = match action {
            0 => GroupOperation::ScrollPause {},
            1 => GroupOperation::ScrollResume {},
            2 => GroupOperation::ScrollRestart {},
            3 => GroupOperation::ScrollSpeed { speed },
            _ => return,
        };
        self.group_apply(op, 0);
    }
}

/// Which source Properties shows. The selection wins; otherwise the last source
/// the panel showed, if it is still in `ids`; otherwise none (`-1`), so a fresh
/// start or a removed source leaves the "Nothing selected" state instead of an
/// arbitrary first source.
fn properties_source(selected: i32, remembered: i32, ids: &[i32]) -> i32 {
    if selected >= 0 && ids.contains(&selected) {
        return selected;
    }
    if remembered >= 0 && ids.contains(&remembered) {
        return remembered;
    }
    -1
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_api::request::{GroupBounds, GroupMemberState};

    #[test]
    fn properties_keeps_the_last_source_when_nothing_is_selected() {
        let ids = [4, 7, 9];
        assert_eq!(properties_source(7, 4, &ids), 7);
        assert_eq!(properties_source(-1, 7, &ids), 7);
        // The remembered source was removed, or none was ever shown: the panel is empty.
        assert_eq!(properties_source(-1, 3, &ids), -1);
        assert_eq!(properties_source(-1, -1, &ids), -1);
        assert_eq!(properties_source(-1, 7, &[]), -1);
    }

    fn member(item: u32, visible: bool, show_for_ms: Option<u32>) -> GroupMemberState {
        GroupMemberState {
            item,
            visible,
            locked: false,
            show_for_ms,
            cued: false,
            remaining_ms: None,
            has_picture: true,
        }
    }

    fn state(members: Vec<GroupMemberState>) -> (Target, GroupState) {
        let t = Target {
            scene: 1,
            group: 2,
            items: members.iter().map(|m| m.item).collect(),
            canvas: OutputCanvas::Main,
            can_vertical: false,
            anchor: 5,
            members: Vec::new(),
        };
        let bounds = Some(GroupBounds {
            x: 1.0,
            y: 2.0,
            width: 3.0,
            height: 4.0,
        });
        let s = GroupState {
            scene: 1,
            group: 2,
            name: "Pair".into(),
            items: t.items.clone(),
            members,
            canvas: OutputCanvas::Main,
            bounds,
            scroll_bounds: bounds,
            scroll: None,
            run: None,
            render_failures: Vec::new(),
        };
        (t, s)
    }

    #[test]
    fn a_hidden_properties_panel_reads_the_group_only_once_it_shows() {
        let (mut ui, d) = crate::studio_ui::tests::ui("group-props-hidden");
        let (t, _) = state(vec![member(1, true, None), member(2, true, None)]);
        ui.retarget(Some(t.clone()));
        assert_eq!(ui.props_target.asked, None, "hidden: nothing is asked");
        ui.set_properties_shown(true);
        let first = ui.props_target.asked;
        assert!(first.is_some(), "shown: the waiting read is made");
        ui.set_properties_shown(false);
        ui.set_properties_shown(true);
        assert_eq!(ui.props_target.asked, first, "no change: no second read");
        let moved = Target {
            canvas: OutputCanvas::Vertical,
            ..t
        };
        ui.retarget(Some(moved));
        assert_ne!(
            ui.props_target.asked, first,
            "shown: a change is read at once"
        );
        drop(ui);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_duration_set_on_some_members_only_is_mixed() {
        let (t, s) = state(vec![member(1, true, Some(500)), member(2, true, None)]);
        let d = props_data(&t, &s, 1);
        assert!(d.show_mixed);
        assert_eq!(d.show_for_ms, 0);
        let (t, s) = state(vec![member(1, true, Some(500)), member(2, true, Some(500))]);
        let d = props_data(&t, &s, 1);
        assert!(!d.show_mixed);
        assert_eq!(d.show_for_ms, 500);
    }

    #[test]
    fn show_says_why_it_cannot_start() {
        let (t, s) = state(vec![member(1, true, None), member(2, true, None)]);
        assert_eq!(props_data(&t, &s, 1).show_why, 1, "every member is shown");
        let (t, s) = state(vec![member(1, false, Some(9)), member(2, false, None)]);
        assert_eq!(
            props_data(&t, &s, 1).show_why,
            2,
            "a hidden member has none"
        );
        let (t, s) = state(vec![member(1, false, Some(9)), member(2, true, None)]);
        assert_eq!(props_data(&t, &s, 1).show_why, 0);
    }

    #[test]
    fn a_locked_member_disables_the_box_and_a_member_without_picture_disables_scroll() {
        let (t, mut s) = state(vec![member(1, true, None), member(2, true, None)]);
        assert_eq!(props_data(&t, &s, 1).box_why, 0);
        s.members[0].locked = true;
        s.members[1].has_picture = false;
        let d = props_data(&t, &s, 1);
        assert_eq!((d.box_why, d.scroll_why), (1, 1));
        s.members[0].locked = false;
        s.bounds = None;
        assert_eq!(props_data(&t, &s, 1).box_why, 2);
    }
}
