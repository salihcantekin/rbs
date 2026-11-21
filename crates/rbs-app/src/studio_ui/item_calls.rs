//! Item operations of the window, sent as control API commands (ADR-0163 step 2):
//! the Sources row's eye, lock and layer buttons, Add existing source, and the
//! timed show of an item, and Undo and Redo.

use rbs_api::Command as C;
use rbs_api::request as rq;

use rbs_app::studio_host::ApiReply;

use super::StudioUi;

impl StudioUi {
    /// The item of source `id` in the scene being edited: the Sources row's.
    fn row_item(&self, id: i32) -> Option<u32> {
        self.rows
            .iter()
            .find(|r| r.id.get() == id as u32)
            .map(|r| r.item.get())
    }

    /// Pause (0), Resume (1) or Restart (2) of a source's scroll on the canvas
    /// Properties edits: live, not an undo step (`item.scroll_*`).
    pub fn scroll_live(&mut self, id: i32, action: i32) {
        let Some(item) = self.row_item(id) else {
            return;
        };
        let at = rq::ScrollItem {
            item,
            canvas: self.props_target(id),
        };
        self.call(match action {
            0 => C::ItemScrollPause(at),
            1 => C::ItemScrollResume(at),
            _ => C::ItemScrollRestart(at),
        });
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

    /// Undo (`back`) or redo the newest scene edit (Ctrl/Cmd+Z, the Undo and Redo
    /// buttons). A box on its way is still applied first: the studio takes commands
    /// in order.
    pub fn undo_edit(&mut self, back: bool) {
        self.deleted.clear();
        let call = if back { C::EditUndo } else { C::EditRedo };
        // Nothing to undo says nothing (the buttons are disabled then); a refused
        // step is shown.
        self.call_as(call, ApiReply::QuietIf(rbs_api::ErrorKind::Conflict));
    }
}

impl StudioUi {
    /// `item.set_style` for the row's item on the canvas Properties edits.
    pub(super) fn send_style(&self, id: i32, style: &rbs_scene::model::ItemStyle, gesture: u64) {
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemSetStyle(rq::SetItemStyle {
                item,
                style: rbs_app::studio_host::style_for_api(style),
                canvas: self.props_target(id),
                gesture,
            }));
        }
    }

    /// `item.set_scroll` for the row's item on the canvas Properties edits; `None`
    /// stops it.
    pub(super) fn send_scroll(
        &self,
        id: i32,
        scroll: Option<&rbs_scene::model::Scroll>,
        gesture: u64,
    ) {
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemSetScroll(rq::SetItemScroll {
                item,
                scroll: scroll.map(rbs_app::studio_host::scroll_for_api),
                canvas: self.props_target(id),
                gesture,
            }));
        }
    }
}
