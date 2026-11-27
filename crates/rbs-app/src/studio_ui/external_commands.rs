//! Read-only resolution of bulk commands for an external caller's API session.

use super::*;
use crate::menu_bar::{GroupOp, MixerBulk};
use rbs_api::request as rq;
use rbs_api::{ApiError, Command, ErrorKind};

impl StudioUi {
    /// Starts the API thread and attaches it to the control thread, with local IPC
    /// when it can be served (so `rbs-cli token` changes reach this app at once).
    /// False (the reason in `remote_failure`) when it did not start.
    pub(super) fn start_api(&mut self) -> bool {
        let Some(host) = &self.host else {
            return false;
        };
        let forward = self.ai_forward.clone();
        let flags = self.slot_flags.clone();
        let options = RemoteOptions {
            control_waker: Box::new(host.api_waker()),
            audit: Box::new(move |a| {
                forward.record(&a, unix_now(), flags.ai_tools_started());
                rbs_core::debug!(Area::Remote, "{a:?}");
            }),
            ipc: None,
            levels: Some(host.level_feed()),
        };
        self.slot_flags.set(self.ai_started, self.remote.enabled);
        let slot = self
            .slot_flags
            .slot(ai_settings::union_store(&self.remote, &self.ai));
        let (mut server, endpoint) = match RemoteServer::start_api(slot, options) {
            Ok(started) => started,
            Err(e) => {
                rbs_core::error!(Area::Remote, "did not start: {e}");
                self.remote_failure = Some(format!("Remote control did not start: {e}"));
                return false;
            }
        };
        match rbs_api_ipc::Endpoint::for_current_user() {
            Ok(e) => {
                let shown = e.to_string();
                match server.start_ipc(e) {
                    Ok(()) => rbs_core::info!(Area::Remote, "local IPC at {shown}"),
                    Err(why) => rbs_core::warn!(Area::Remote, "no local IPC: {why}"),
                }
            }
            Err(why) => rbs_core::warn!(Area::Remote, "no local IPC endpoint: {why}"),
        }
        if let Err(error) = crate::ui_commands::register(server.ui_router()) {
            rbs_core::error!(Area::Remote, "UI client registration failed: {error}");
            self.remote_failure = Some(error.message);
            return false;
        }
        host.attach_api(
            endpoint,
            Some(server.api_tokens(self.config.clone(), self.slot_flags.clone())),
        );
        self.remote_server = Some(server);
        true
    }

    pub(crate) fn external_group_calls(
        &self,
        group: i32,
        op: GroupOp,
    ) -> Result<Vec<Command>, ApiError> {
        let members: Vec<_> = self
            .rows
            .iter()
            .filter(|row| row.group.is_some_and(|id| id.get() == group as u32))
            .collect();
        if members.is_empty() {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                "group is no longer in this scene",
            ));
        }
        let gesture = next_gesture();
        match op {
            GroupOp::Visible(visible) => Ok(members
                .iter()
                .map(|row| {
                    Command::ItemSetVisible(rq::SetItemVisible {
                        item: row.item.get(),
                        visible,
                        gesture,
                    })
                })
                .collect()),
            GroupOp::Locked(locked) => Ok(members
                .iter()
                .filter(|row| row.layout.is_some())
                .map(|row| {
                    Command::ItemSetLocked(rq::SetItemLocked {
                        item: row.item.get(),
                        locked,
                        gesture,
                    })
                })
                .collect()),
            GroupOp::Rename | GroupOp::Remove => Err(ApiError::new(
                ErrorKind::InvalidParams,
                "this group command needs input at the window",
            )),
        }
    }

    pub(crate) fn external_mixer_calls(&self, bulk: MixerBulk) -> Vec<Command> {
        let (hidden, muted_only) = match bulk {
            MixerBulk::HideAll => (true, false),
            MixerBulk::HideMuted => (true, true),
            MixerBulk::ShowAll => (false, false),
        };
        self.heard
            .iter()
            .filter(|row| row.hidden != hidden && (!muted_only || row.muted))
            .map(|row| {
                Command::SourceSetMixerHidden(rq::SetSourceMixerHidden {
                    source: row.id,
                    hidden,
                })
            })
            .collect()
    }
}

fn next_gesture() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
