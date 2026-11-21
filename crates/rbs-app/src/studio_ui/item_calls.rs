//! Item operations of the window, sent as control API commands (ADR-0163 step 2):
//! the Sources row's eye, lock and layer buttons, Add existing source, and the
//! timed show of an item.

use rbs_api::Command as C;
use rbs_api::request as rq;

use super::StudioUi;

impl StudioUi {
    /// The item of source `id` in the scene being edited: the Sources row's.
    fn row_item(&self, id: i32) -> Option<u32> {
        self.rows
            .iter()
            .find(|r| r.id.get() == id as u32)
            .map(|r| r.item.get())
    }

    pub fn set_source_locked(&mut self, id: i32, locked: bool) {
        self.deleted.clear();
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemSetLocked(rq::SetItemLocked { item, locked }));
        }
    }

    /// Show or hide a source in the program scene (the Sources row's eye button).
    pub fn set_source_visible(&mut self, id: i32, visible: bool) {
        self.deleted.clear();
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemSetVisible(rq::SetItemVisible { item, visible }));
        }
    }

    /// One layer up (drawn over the source above it in the list) or down.
    pub fn move_source(&mut self, id: i32, direction: rq::LayerDirection) {
        self.deleted.clear();
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemMove(rq::MoveItem { item, direction }));
        }
    }

    /// Remove from scene (ADR-0065): the Sources row's remove button and Delete key.
    pub fn remove_from_scene(&mut self, id: i32) {
        self.deleted.clear();
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemRemove(rq::ItemRef { item }));
        }
    }

    /// Places a source of another scene into the scene on air (Add source, Existing
    /// source); it is selected once it is there.
    pub fn add_existing_source(&mut self, id: i32) {
        self.deleted.clear();
        self.adding = true;
        self.call(C::ItemAdd(rq::AddItem {
            source: id as u32,
            scene: None,
        }));
    }

    /// Stored timed-show duration on the item (ADR-0105). `show_for_ms` 0 clears.
    pub fn set_item_show_for(&mut self, item: i32, show_for_ms: i32) {
        let show_for_ms = if show_for_ms <= 0 {
            None
        } else {
            Some((show_for_ms as u32).min(rbs_scene::MAX_SHOW_FOR_MS))
        };
        self.call(C::ItemSetShowFor(rq::SetItemShowFor {
            item: item as u32,
            show_for_ms,
        }));
    }

    /// Live timed show for a hidden item (ADR-0105).
    pub fn show_item(&mut self, item: i32) {
        self.call(C::ItemShow(rq::ShowItem { item: item as u32 }));
    }
}
