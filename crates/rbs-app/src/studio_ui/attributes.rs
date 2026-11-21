//! The Paste attributes and Reset source windows' state (ADR-0133 decision 8): which
//! groups and sources are checked, what is enabled, and the command the confirm button
//! sends. The host answers what the window shows ([`AttributesView`]); the checks live
//! here until the window closes.

use rbs_api::Command as C;
use rbs_api::request as rq;
use rbs_api::types::{FilterPasteMode, OutputCanvas};
use rbs_app::studio_host::AttributesView;
use rbs_scene::AttributeGroup;
use slint::{ModelRc, SharedString, VecModel};

use crate::ui::{AttributeGroupRow, AttributeTargetRow, AttributesData};

/// One open Paste attributes or Reset source window.
#[derive(Debug, Clone)]
pub(crate) struct Dialog {
    view: AttributesView,
    /// Checked, per entry of `view.groups`.
    groups: Vec<bool>,
    /// Checked, per entry of `view.targets`.
    targets: Vec<bool>,
    same_type: bool,
    append: bool,
}

/// The section a group is listed under: 0 Picture, 1 Sound, 2 Source.
fn section(g: AttributeGroup) -> i32 {
    match g {
        AttributeGroup::Transform
        | AttributeGroup::Crop
        | AttributeGroup::Opacity
        | AttributeGroup::Style
        | AttributeGroup::Scroll
        | AttributeGroup::VideoFilters => 0,
        AttributeGroup::AudioFilters | AttributeGroup::Sound | AttributeGroup::SyncOffset => 1,
        AttributeGroup::Options
        | AttributeGroup::Region
        | AttributeGroup::Glide
        | AttributeGroup::RecordSeparately => 2,
    }
}

impl Dialog {
    /// The window's first state. A reset checks the groups that apply and differ from
    /// their default; a paste checks `remembered` (the groups chosen last in this
    /// session) or else the copied groups that differ from the default, and the
    /// right-clicked source's first item. Live groups always start unchecked.
    pub(crate) fn new(view: AttributesView, remembered: Option<&[AttributeGroup]>) -> Self {
        let groups = view
            .groups
            .iter()
            .map(|(g, differs, applies)| {
                !g.is_live()
                    && match (view.paste, remembered) {
                        (true, Some(r)) => r.contains(g),
                        (true, None) => *differs,
                        (false, _) => *differs && *applies,
                    }
            })
            .collect();
        let first = view.targets.iter().position(|t| t.source == view.source);
        let targets = (0..view.targets.len()).map(|i| Some(i) == first).collect();
        Self {
            view,
            groups,
            targets,
            same_type: false,
            append: false,
        }
    }

    fn shown(&self, i: usize) -> bool {
        !self.same_type || self.view.targets[i].same_type
    }

    /// The checked sources to paste onto.
    fn chosen_targets(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.targets.len()).filter(|i| self.targets[*i] && self.shown(*i))
    }

    /// Whether group `i` can be chosen: it applies to the source reset, or to a checked
    /// source to paste onto.
    fn available(&self, i: usize) -> bool {
        let (g, _, applies) = self.view.groups[i];
        if !self.view.paste {
            return applies;
        }
        self.chosen_targets()
            .any(|t| self.view.targets[t].applies.contains(&g))
    }

    /// The groups the confirm button sends.
    pub(crate) fn chosen_groups(&self) -> Vec<AttributeGroup> {
        (0..self.groups.len())
            .filter(|i| self.groups[*i] && self.available(*i))
            .map(|i| self.view.groups[i].0)
            .collect()
    }

    pub(crate) fn set_group(&mut self, i: usize, on: bool) {
        if let Some(g) = self.groups.get_mut(i) {
            *g = on;
        }
    }

    pub(crate) fn set_target(&mut self, i: usize, on: bool) {
        if let Some(t) = self.targets.get_mut(i) {
            *t = on;
        }
    }

    pub(crate) fn set_same_type(&mut self, on: bool) {
        self.same_type = on;
    }

    pub(crate) fn set_append(&mut self, on: bool) {
        self.append = on;
    }

    /// True for a paste.
    pub(crate) fn paste(&self) -> bool {
        self.view.paste
    }

    /// What the confirm button sends, if anything is chosen: `source.reset` or
    /// `source.paste_attributes` (ADR-0163).
    pub(crate) fn command(&self) -> Option<C> {
        let groups = self.chosen_groups();
        if groups.is_empty() {
            return None;
        }
        let groups = groups.iter().map(|g| g.id().to_owned()).collect();
        if !self.view.paste {
            return Some(C::SourceReset(rq::ResetSource {
                source: self.view.source.get(),
                scene: None,
                canvas: OutputCanvas::default(),
                groups: Some(groups),
            }));
        }
        let items: Vec<_> = self
            .chosen_targets()
            .map(|i| self.view.targets[i].item.get())
            .collect();
        if items.is_empty() {
            return None;
        }
        Some(C::SourcePasteAttributes(rq::PasteAttributes {
            items,
            groups,
            filters: if self.append {
                FilterPasteMode::Append
            } else {
                FilterPasteMode::Replace
            },
            canvas: OutputCanvas::default(),
        }))
    }

    fn help(&self, i: usize) -> &'static str {
        let (g, _, _) = self.view.groups[i];
        match (self.available(i), self.view.paste, g) {
            (false, true, _) => "None of the checked sources takes this.",
            (false, false, _) => "This source has none.",
            (true, _, AttributeGroup::Sound) => {
                "Not undone by Undo. In a scene with its own levels, the scene's level is heard."
            }
            (true, _, AttributeGroup::SyncOffset) => "Not undone by Undo.",
            (true, _, AttributeGroup::Options) => "The source opens again with these options.",
            _ => "",
        }
    }

    /// What the window shows.
    pub(crate) fn data(&self) -> AttributesData {
        let rows: Vec<(i32, AttributeGroupRow)> = self
            .view
            .groups
            .iter()
            .enumerate()
            .map(|(i, (g, differs, _))| {
                (
                    section(*g),
                    AttributeGroupRow {
                        id: g.id().into(),
                        title: g.title().into(),
                        index: i as i32,
                        checked: self.groups[i],
                        enabled: self.available(i),
                        at_default: !differs,
                        help: self.help(i).into(),
                    },
                )
            })
            .collect();
        let section_rows = |n: i32| {
            ModelRc::new(VecModel::from(
                rows.iter()
                    .filter(|(s, _)| *s == n)
                    .map(|(_, r)| r.clone())
                    .collect::<Vec<_>>(),
            ))
        };
        let targets: Vec<AttributeTargetRow> = self
            .view
            .targets
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut note = Vec::new();
                if t.place > 0 {
                    note.push(t.place.to_string());
                }
                if t.global {
                    note.push("global".to_owned());
                }
                if t.copied {
                    note.push("copied".to_owned());
                }
                AttributeTargetRow {
                    name: t.name.as_str().into(),
                    note: SharedString::from(note.join(", ")),
                    checked: self.targets[i] && self.shown(i),
                    copied: t.copied,
                    shown: self.shown(i),
                }
            })
            .collect();
        let chosen = self.chosen_groups();
        AttributesData {
            paste: self.view.paste,
            name: self.view.name.as_str().into(),
            picture: section_rows(0),
            sound: section_rows(1),
            source: section_rows(2),
            targets: ModelRc::new(VecModel::from(targets)),
            same_type: self.same_type,
            append: self.append,
            count: self.chosen_targets().count() as i32,
            groups_on: chosen.len() as i32,
            has_filters: chosen.iter().any(|g| {
                matches!(
                    g,
                    AttributeGroup::VideoFilters | AttributeGroup::AudioFilters
                )
            }),
        }
    }

    /// The non-live groups to remember for the next paste in this session.
    pub(crate) fn remembered(&self) -> Vec<AttributeGroup> {
        self.chosen_groups()
            .into_iter()
            .filter(|g| !g.is_live())
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)] // Tests fail by panicking.
mod tests {
    use super::*;
    use rbs_app::studio_host::AttributeTargetView;
    use rbs_scene::model::{ItemId, SourceId};

    fn view(paste: bool) -> AttributesView {
        AttributesView {
            paste,
            source: SourceId::from_raw(2),
            name: "Camera".into(),
            groups: AttributeGroup::ALL
                .into_iter()
                .map(|g| {
                    let differs = matches!(
                        g,
                        AttributeGroup::Crop | AttributeGroup::Sound | AttributeGroup::Options
                    );
                    (g, differs, g != AttributeGroup::Region)
                })
                .collect(),
            targets: vec![
                target(10, 1, true, vec![AttributeGroup::Crop]),
                target(
                    11,
                    2,
                    true,
                    vec![AttributeGroup::Crop, AttributeGroup::Options],
                ),
                target(12, 3, false, vec![AttributeGroup::Sound]),
            ],
        }
    }

    fn target(
        item: u32,
        source: u32,
        same_type: bool,
        applies: Vec<AttributeGroup>,
    ) -> AttributeTargetView {
        AttributeTargetView {
            item: ItemId::from_raw(item),
            source: SourceId::from_raw(source),
            name: format!("S{source}"),
            same_type,
            copied: source == 1,
            global: false,
            place: 0,
            applies,
        }
    }

    #[test]
    fn reset_checks_what_differs_and_applies_never_live() {
        let d = Dialog::new(view(false), None);
        assert_eq!(
            d.chosen_groups(),
            [AttributeGroup::Crop, AttributeGroup::Options]
        );
        match d.command() {
            Some(C::SourceReset(p)) => {
                assert_eq!(p.source, 2);
                let groups = p.groups.expect("the chosen groups");
                assert!(!groups.iter().any(|g| g == AttributeGroup::Sound.id()));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn paste_follows_checked_targets_and_same_type() {
        let mut d = Dialog::new(view(true), None);
        // The right-clicked source (2) is checked; Crop and Options apply to it.
        assert_eq!(
            d.chosen_groups(),
            [AttributeGroup::Crop, AttributeGroup::Options]
        );
        // Sound is live: never checked by default, even with a target that takes it.
        d.set_target(2, true);
        assert!(!d.chosen_groups().contains(&AttributeGroup::Sound));
        let sound = AttributeGroup::ALL
            .iter()
            .position(|g| *g == AttributeGroup::Sound)
            .expect("Sound is a group");
        d.set_group(sound, true);
        assert!(d.chosen_groups().contains(&AttributeGroup::Sound));
        // Same type only hides and drops source 3, and with it Sound.
        d.set_same_type(true);
        assert!(!d.chosen_groups().contains(&AttributeGroup::Sound));
        let data = d.data();
        assert_eq!(data.count, 1);
        match d.command() {
            Some(C::SourcePasteAttributes(p)) => {
                assert_eq!(p.items, [11]);
                assert_eq!(p.filters, FilterPasteMode::Replace);
            }
            other => panic!("{other:?}"),
        }
        // Nothing checked to paste onto: no command.
        d.set_target(1, false);
        assert!(d.command().is_none());
    }

    #[test]
    fn remembered_groups_start_checked_without_live_ones() {
        let d = Dialog::new(
            view(true),
            Some(&[AttributeGroup::Crop, AttributeGroup::Sound]),
        );
        assert_eq!(d.chosen_groups(), [AttributeGroup::Crop]);
        assert_eq!(d.remembered(), [AttributeGroup::Crop]);
    }
}
