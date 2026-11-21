//! Output operations of the window, sent as control API commands (ADR-0163 step 6):
//! adding and removing the vertical canvas; and which outputs Record and Go live
//! act on.

use rbs_api::Command as C;
use rbs_api::request as rq;
use rbs_app::studio_host::Command;
use rbs_app::vertical_outputs::{self, VerticalFlags};

use super::StudioUi;

impl StudioUi {
    /// Add vertical canvas: `layout` is the prompt's choice, `0` none, then Fit,
    /// Fill and Stack. The prompt checked the size.
    pub fn add_vertical(&self, width: u32, height: u32, follow: bool, layout: i32) {
        let layout = match layout {
            1 => Some(rq::VerticalLayout::Fit),
            2 => Some(rq::VerticalLayout::Fill),
            3 => Some(rq::VerticalLayout::Stack),
            _ => None,
        };
        self.call(C::CanvasAddVertical(rq::AddVerticalCanvas {
            width: Some(width),
            height: Some(height),
            follow: Some(follow),
            layout,
        }));
    }

    /// Remove vertical canvas.
    pub fn remove_vertical(&self) {
        self.call(C::CanvasRemoveVertical(rq::Empty {}));
    }

    /// Keeps and saves whether Record and Go live also act on the vertical outputs
    /// (ADR-0111), and tells the host.
    pub fn set_vertical_flags(&mut self, flags: VerticalFlags) {
        self.vertical_flags = flags;
        if let Err(why) = self.user.set_section(vertical_outputs::SECTION, &flags) {
            self.profile_note = format!("The output choice was not saved: {why}");
        }
        self.send(Command::SetVerticalFlags(flags));
    }
}
