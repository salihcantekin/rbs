//! The control thread's command channel (ADR-0154).

use std::sync::mpsc::{SendError, Sender};
use std::sync::{Arc, OnceLock};

use super::Msg;

/// The control thread's command channel. Sending also unparks that thread once it
/// has bound itself (ADR-0154). A bare channel send does not wake a `park`.
pub(crate) struct ControlOut {
    pub(super) tx: Sender<Msg>,
    pub(super) wake: OnceLock<Arc<rbs_graph::runtime::ControlWake>>,
}

impl ControlOut {
    // The same shape as `Sender::send`: the error hands the message back, so it is as
    // large as `Msg`. Callers only test it; nothing moves it around.
    #[allow(clippy::result_large_err)]
    pub(super) fn send(&self, msg: Msg) -> Result<(), SendError<Msg>> {
        let sent = self.tx.send(msg);
        if let Some(wake) = self.wake.get() {
            wake.unpark();
        }
        sent
    }
}
