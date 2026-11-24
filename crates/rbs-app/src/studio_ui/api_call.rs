//! How the window sends to the studio host: its own commands, and control API
//! commands run as a remote call would be (ADR-0163 decision 1).

use std::sync::atomic::{AtomicU64, Ordering};

use rbs_app::studio_host::{ApiReply, Command};

use super::StudioUi;

impl StudioUi {
    /// Sends a window command (a window-only one, or one not moved yet; ADR-0163).
    pub fn send(&self, c: Command) {
        if let Some(h) = &self.host {
            h.send(c);
        }
    }

    /// Arranges the scene the vertical canvas shows (`canvas.auto_layout`).
    pub fn auto_layout_vertical(&self, mode: rbs_api::request::VerticalLayout) {
        self.send(Command::AutoLayoutVertical(mode));
    }

    /// Runs a control API command on the control thread; a refusal shows as the error
    /// bar and state comes back through the usual updates (ADR-0163 decision 2).
    pub fn call(&self, call: rbs_api::Command) {
        self.call_as(call, ApiReply::Shown);
    }

    /// As [`StudioUi::call`], answered as `reply` asks.
    pub fn call_as(&self, call: rbs_api::Command, reply: ApiReply) {
        self.send(Command::Api {
            call: Box::new(call),
            reply,
        });
    }

    /// Asks a control API question whose answer comes back as an
    /// `Update::ApiAnswer` with the returned ticket (ADR-0163 decision 2).
    pub fn ask(&self, call: rbs_api::Command) -> u64 {
        let ticket = next_ticket();
        self.call_as(call, ApiReply::Ticket(ticket));
        ticket
    }

    /// As [`StudioUi::ask`], but a refusal is not shown: the asker handles it.
    pub fn ask_quietly(&self, call: rbs_api::Command) -> u64 {
        let ticket = next_ticket();
        self.call_as(call, ApiReply::QuietTicket(ticket));
        ticket
    }
}

/// A ticket unique for the process: an answer is matched to its question only.
fn next_ticket() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
