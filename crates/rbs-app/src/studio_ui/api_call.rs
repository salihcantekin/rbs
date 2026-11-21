//! How the window sends to the studio host: its own commands, and control API
//! commands run as a remote call would be (ADR-0163 decision 1).

use rbs_app::studio_host::{ApiReply, Command};

use super::StudioUi;

impl StudioUi {
    /// Sends a window command (a window-only one, or one not moved yet; ADR-0163).
    pub fn send(&self, c: Command) {
        if let Some(h) = &self.host {
            h.send(c);
        }
    }

    /// Runs a control API command on the control thread; a refusal shows as the error
    /// bar and state comes back through the usual updates (ADR-0163 decision 2).
    pub fn call(&self, call: rbs_api::Command) {
        self.send(Command::Api {
            call: Box::new(call),
            reply: ApiReply::Shown,
        });
    }
}
