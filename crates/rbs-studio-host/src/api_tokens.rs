//! The control API's `tokens.*` answers for the control thread, and the token slot's
//! AI tools flags (ADR-0134 decision 2). The server that holds the tokens is
//! `rbs-app`'s `remote_control`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rbs_api::request::Response;
use rbs_api::{ApiError, Command, TokenHandle, TokenSlot, TokenStore};

/// The control API's `tokens.*` methods for the control thread: the settings in
/// `config` (`rbs_remote_settings::token_command`) and the running server's tokens.
#[derive(Debug, Clone)]
pub struct ApiTokens {
    config: PathBuf,
    handle: TokenHandle,
    flags: Arc<SlotFlags>,
}

/// The AI tools flags of the token slot, shared by the app and the control thread's
/// `tokens.*` answers (ADR-0134 decision 2: one composer builds every slot value).
#[derive(Debug)]
pub struct SlotFlags {
    ai_tools_started: AtomicBool,
    tokenless_ipc: AtomicBool,
}

impl Default for SlotFlags {
    fn default() -> Self {
        Self {
            ai_tools_started: AtomicBool::new(false),
            tokenless_ipc: AtomicBool::new(true),
        }
    }
}

impl SlotFlags {
    /// Sets both flags.
    pub fn set(&self, ai_tools_started: bool, tokenless_ipc: bool) {
        self.ai_tools_started
            .store(ai_tools_started, Ordering::Release);
        self.tokenless_ipc.store(tokenless_ipc, Ordering::Release);
    }

    /// AI tools are started.
    pub fn ai_tools_started(&self) -> bool {
        self.ai_tools_started.load(Ordering::Acquire)
    }

    /// The slot value of `store` with the flags as they are now.
    pub fn slot(&self, store: TokenStore) -> TokenSlot {
        TokenSlot {
            store,
            ai_tools_started: self.ai_tools_started.load(Ordering::Acquire),
            tokenless_ipc: self.tokenless_ipc.load(Ordering::Acquire),
        }
    }
}

impl ApiTokens {
    /// The `tokens.*` answers for the settings in `config` and the server's `handle`.
    pub fn new(config: PathBuf, handle: TokenHandle, flags: Arc<SlotFlags>) -> Self {
        Self {
            config,
            handle,
            flags,
        }
    }

    /// Answers `command` if it is a `tokens.*` command, `None` otherwise. A change is
    /// saved and the server takes the new tokens (under the settings lock) before this
    /// returns, so a new token works, or a revoked token's apps are disconnected,
    /// before the reply is sent. `true` with the reply when the tokens changed.
    pub fn answer(&self, command: &Command) -> Option<Result<(Response, bool), ApiError>> {
        let answer = rbs_remote_settings::token_command(&self.config, command, |s| {
            self.handle.set(self.flags.slot(s));
        })?;
        Some(answer.map(|a| (a.response, a.changed)))
    }
}
