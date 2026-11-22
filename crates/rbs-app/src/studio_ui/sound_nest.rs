//! The Sources list shows a "Capture audio too" companion under its picture
//! (ADR-0173): the rows are ordered so each companion follows its picture in the same
//! group (Global, or This scene), and marked so the list can draw the disclosure
//! arrow, the indent and the closed row's muted mark. Runs only when the rows are
//! made again or a row is opened or closed; nothing here runs per frame.

use std::collections::BTreeSet;

use slint::Model as _;

use super::{SourceData, SourceRow, StudioUi};

/// What nesting reads of a row: its id, the picture it is the companion of, and
/// whether it is a global source's row.
pub(super) type Key = (u32, Option<u32>, bool);

/// The nesting key of a host row.
pub(super) fn key(r: &SourceRow) -> Key {
    (r.id.get(), r.companion_of.map(|p| p.get()), r.global)
}

/// The index of the picture row that row `i` nests under: its companion picture, in
/// the same group, itself no companion.
fn parent_of(keys: &[Key], i: usize) -> Option<usize> {
    let (_, of, group) = keys[i];
    let of = of?;
    keys.iter()
        .position(|(id, parent, g)| *id == of && *g == group && parent.is_none())
}

/// `items` with each nested companion moved directly under its picture, several in
/// their item order; every other row keeps its place.
pub(super) fn order<T>(items: Vec<T>, key: impl Fn(&T) -> Key) -> Vec<T> {
    let keys: Vec<Key> = items.iter().map(key).collect();
    let parents: Vec<Option<usize>> = (0..keys.len()).map(|i| parent_of(&keys, i)).collect();
    let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(slots.len());
    for i in 0..slots.len() {
        if parents[i].is_some() {
            continue;
        }
        out.extend(slots[i].take());
        for j in 0..slots.len() {
            if parents[j] == Some(i) {
                out.extend(slots[j].take());
            }
        }
    }
    out
}

/// The pictures whose sound rows are open, by source id. In memory only, closed by
/// default (ADR-0173 decision 3).
#[derive(Debug, Default)]
pub(super) struct Open(BTreeSet<i32>);

impl Open {
    /// Marks `data` (ordered by [`order`], `keys` in the same order): the nested rows,
    /// the arrows and the open state. Forgets sources that are gone and opens the
    /// picture of a `selected` companion.
    pub(super) fn mark(&mut self, data: &mut [SourceData], keys: &[Key], selected: i32) {
        let parents: Vec<Option<usize>> = (0..keys.len()).map(|i| parent_of(keys, i)).collect();
        self.0.retain(|id| keys.iter().any(|k| k.0 as i32 == *id));
        if let Some(p) = keys
            .iter()
            .position(|k| k.0 as i32 == selected)
            .and_then(|i| parents[i])
        {
            self.0.insert(keys[p].0 as i32);
        }
        for i in 0..data.len().min(keys.len()) {
            let kids: Vec<usize> = (0..parents.len())
                .filter(|j| parents[*j] == Some(i))
                .collect();
            let muted = kids.iter().any(|j| data[*j].muted);
            let parent = parents[i].map(|p| (data[p].id, data[p].name.clone()));
            let d = &mut data[i];
            d.companion = keys.iter().any(|k| k.1 == Some(keys[i].0));
            d.expandable = !kids.is_empty();
            d.sound_muted = muted;
            d.nested = parent.is_some();
            d.open = self.0.contains(&parent.as_ref().map_or(d.id, |p| p.0));
            (d.parent, d.parent_name) = parent.unwrap_or((-1, Default::default()));
        }
        for global in [true, false] {
            let group = |i: &usize| keys.get(*i).is_some_and(|k| k.2 == global);
            let room = (0..data.len()).filter(group).any(|i| data[i].expandable);
            for i in (0..data.len()).filter(group) {
                data[i].arrow_room = room;
            }
        }
    }
}

impl StudioUi {
    /// Opens or closes the sound rows under picture `id` (its disclosure arrow).
    pub fn open_sound(&mut self, id: i32, open: bool) {
        if open {
            self.nest.0.insert(id);
        } else {
            self.nest.0.remove(&id);
        }
        self.mark_nest();
    }

    /// A selected companion's picture opens, so the selection is in the list.
    pub fn reveal_sound(&mut self) {
        let keys: Vec<Key> = self.rows.iter().map(key).collect();
        let shut = keys
            .iter()
            .position(|k| k.0 as i32 == self.selected)
            .and_then(|i| parent_of(&keys, i))
            .is_some_and(|p| !self.nest.0.contains(&(keys[p].0 as i32)));
        if shut {
            self.mark_nest();
        }
    }

    fn mark_nest(&mut self) {
        let mut data: Vec<SourceData> = self.sources.iter().collect();
        let keys: Vec<Key> = self.rows.iter().map(key).collect();
        self.nest.mark(&mut data, &keys, self.selected);
        super::set_rows(&self.sources, data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i32) -> SourceData {
        SourceData {
            id,
            ..SourceData::default()
        }
    }

    fn ids(keys: &[Key]) -> Vec<u32> {
        order(keys.to_vec(), |k| *k).iter().map(|k| k.0).collect()
    }

    #[test]
    fn a_companion_moves_under_its_picture() {
        // Item order: sound first, then a text, then the window.
        let keys = [(3, Some(2), false), (1, None, false), (2, None, false)];
        assert_eq!(ids(&keys), [1, 2, 3]);
    }

    #[test]
    fn several_companions_keep_their_item_order() {
        let keys = [
            (2, None, false),
            (5, Some(2), false),
            (4, Some(2), false),
            (1, None, false),
        ];
        assert_eq!(ids(&keys), [2, 5, 4, 1]);
    }

    #[test]
    fn a_companion_of_a_picture_in_another_group_stays_on_its_own() {
        // The picture is global, its sound is in This scene.
        let keys = [(2, None, true), (1, None, false), (3, Some(2), false)];
        assert_eq!(ids(&keys), [2, 1, 3]);
        let mut data = [row(2), row(1), row(3)];
        Open::default().mark(&mut data, &keys, -1);
        assert!(!data[2].nested && !data[0].expandable);
        assert!(
            data[0].companion,
            "Properties still shows Capture audio too"
        );
    }

    #[test]
    fn a_companion_whose_picture_is_not_listed_stays_on_its_own() {
        // The picture is only in a nested scene: it has no row here.
        let keys = [(1, None, false), (3, Some(9), false)];
        assert_eq!(ids(&keys), [1, 3]);
        let mut data = [row(1), row(3)];
        Open::default().mark(&mut data, &keys, -1);
        assert!(!data[1].nested && !data[0].arrow_room);
    }

    #[test]
    fn marks_say_what_the_list_draws() {
        let keys = [
            (2, None, false),
            (3, Some(2), false),
            (1, None, false),
            (7, None, true),
        ];
        let mut data = [row(2), row(3), row(1), row(7)];
        data[1].muted = true;
        let mut open = Open::default();
        open.mark(&mut data, &keys, -1);
        assert!(data[0].expandable && !data[0].open && data[0].sound_muted);
        assert!(data[1].nested && data[1].parent == 2 && !data[1].open);
        assert!(data[2].arrow_room && !data[2].expandable);
        assert!(!data[3].arrow_room, "the Global group has no arrow");
        open.0.insert(2);
        open.mark(&mut data, &keys, -1);
        assert!(data[0].open && data[1].open);
    }

    #[test]
    fn a_selected_companion_opens_its_picture_and_gone_sources_are_forgotten() {
        let keys = [(2, None, false), (3, Some(2), false)];
        let mut data = [row(2), row(3)];
        let mut open = Open::default();
        open.0.insert(40);
        open.mark(&mut data, &keys, 3);
        assert!(data[0].open && data[1].open);
        assert_eq!(open.0.iter().copied().collect::<Vec<_>>(), [2]);
    }
}
