//! The control API server: the API thread (`rbs_api::ApiServer`) with two optional,
//! separately started and stopped transports, the WebSocket transport (`rbs_api_ws`,
//! while remote control is on) and the local IPC transport (`rbs_api_ipc`, for
//! `rbs-cli`, `rbs-mcp` and other programs of the same user), wired together here
//! because the app is a composition root (P1, ADR-0044 decision 4). The caller
//! attaches the returned endpoint to the studio's control thread
//! (`StudioHost::attach_api`), with [`ApiTokens`] so it answers `tokens.*`.
//!
//! The split lets a network or certificate change restart only the WebSocket
//! transport, and lets AI tools run the API thread with IPC alone (ADR-0134
//! decisions 2 and 9): sessions on the transport that keeps running stay connected.
//!
//! Nothing here exists while it is not wanted (ADR-0013): the settings hold no
//! server, so no thread, socket or event buffer. With no app connected, the API
//! thread and the transports' I/O threads sleep until a connection arrives.

use std::fmt;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;

use rbs_api::request::ClientInfo;
use rbs_api::{
    ApiOptions, ApiServer, AuditEvent, ControlEndpoint, ControlTransport, ServerConfig, TokenSlot,
    TokenStore,
};

use crate::levels::{LevelFeed, LevelSampler};
use rbs_api_ipc::{Endpoint, IpcConfig, IpcTransport};
use rbs_api_ws::{TlsIdentity, WsConfig, WsTransport};
pub use rbs_studio_host::api_tokens::{ApiTokens, SlotFlags};

/// A running control API server. Dropping it stops the transports (connected apps
/// are told the server is going away), then the API thread.
pub struct RemoteServer {
    // Field order is drop order: the transports stop before the API thread.
    ws: Option<WsTransport>,
    ipc: Option<IpcTransport>,
    /// Why local IPC is not served, when it is not.
    ipc_problem: Option<String>,
    api: ApiServer,
}

/// Why remote control did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// Another program listens on this port.
    PortInUse(u16),
    /// Anything else, in the system's words (for the log and the status line).
    Other(String),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::PortInUse(port) => write!(f, "another app is using port {port}"),
            StartError::Other(why) => f.write_str(why),
        }
    }
}

/// What the server needs from the app.
pub struct RemoteOptions {
    /// Wakes the studio's control thread when a request waits (`StudioHost::api_waker`).
    pub control_waker: Box<dyn Fn() + Send + Sync>,
    /// Logs connections, authentication and privileged calls (never a token).
    pub audit: Box<dyn FnMut(AuditEvent) + Send>,
    /// The local IPC endpoint to serve as well (`Endpoint::for_current_user`), or
    /// `None` for none. Not serving it is not fatal: `RemoteServer::ipc_problem`.
    pub ipc: Option<Endpoint>,
    /// Source meters for `audio.levels`. `None`: that topic sends nothing.
    pub levels: Option<Arc<LevelFeed>>,
}

/// Network access (ADR-0046): other computers connect over TLS.
pub struct NetworkOptions {
    /// What the server presents (`crate::network_access::prepare`).
    pub identity: TlsIdentity,
    /// Names clients may use in `Host` besides addresses: this computer's name and
    /// `<name>.local` (`crate::network_access::host_names`).
    pub host_names: Vec<String>,
    /// "Allow remote admin": sessions from other computers keep `admin`.
    pub allow_remote_admin: bool,
}

impl RemoteServer {
    /// Register desktop clients without adding UI methods to the engine catalogue.
    pub fn ui_router(&self) -> rbs_api::ui::UiRouter {
        self.api.ui_router()
    }
    /// Starts the API thread and listens on `addresses`, which must be loopback. The
    /// returned endpoint goes to the control thread. On failure nothing keeps running.
    pub fn start(
        addresses: Vec<SocketAddr>,
        tokens: TokenStore,
        options: RemoteOptions,
    ) -> Result<(RemoteServer, ControlEndpoint), StartError> {
        Self::start_with(WsConfig::on(addresses), false, tokens, options)
    }

    /// Network access at `port`: every interface, TLS for other computers, plain
    /// WebSocket still for apps on this computer.
    pub fn start_network(
        port: u16,
        network: NetworkOptions,
        tokens: TokenStore,
        options: RemoteOptions,
    ) -> Result<(RemoteServer, ControlEndpoint), StartError> {
        let mut ws = WsConfig::network(port, network.identity);
        ws.policy.host_names = network.host_names;
        Self::start_with(ws, network.allow_remote_admin, tokens, options)
    }

    fn start_with(
        ws: WsConfig,
        allow_remote_admin: bool,
        tokens: TokenStore,
        mut options: RemoteOptions,
    ) -> Result<(RemoteServer, ControlEndpoint), StartError> {
        let ipc = options.ipc.take();
        let (mut server, endpoint) = Self::start_api(tokens, options)?;
        server.start_ws(ws, allow_remote_admin)?;
        if let Some(e) = ipc
            && let Err(why) = server.start_ipc(e)
        {
            // Not fatal for remote control: `ipc_problem` says why.
            server.ipc_problem = Some(why);
        }
        Ok((server, endpoint))
    }

    /// Starts the API thread alone, with no transport (`options.ipc` is ignored:
    /// [`RemoteServer::start_ipc`] adds it). The returned endpoint goes to the control
    /// thread.
    pub fn start_api(
        tokens: impl Into<TokenSlot>,
        options: RemoteOptions,
    ) -> Result<(RemoteServer, ControlEndpoint), StartError> {
        let config =
            ServerConfig::new(ClientInfo::new("RBS", env!("CARGO_PKG_VERSION"))).map_err(|e| {
                StartError::Other(format!(
                    "the system could not make a random server id ({e})"
                ))
            })?;
        let mut api_options = ApiOptions::new(options.control_waker);
        api_options.audit = options.audit;
        if let Some(levels) = options.levels {
            api_options.sampler = Some(Box::new(LevelSampler(levels)));
        }
        let (api, endpoint) = ApiServer::start(config, tokens, api_options)
            .map_err(|e| StartError::Other(e.to_string()))?;
        Ok((
            RemoteServer {
                ws: None,
                ipc: None,
                ipc_problem: None,
                api,
            },
            endpoint,
        ))
    }

    /// Starts the WebSocket transport on `ws` (stopping the one that runs, whose
    /// sessions are closed). IPC sessions stay connected. `allow_remote_admin` applies
    /// to the sessions of the new transport. On failure no WebSocket transport runs.
    pub fn start_ws(&mut self, ws: WsConfig, allow_remote_admin: bool) -> Result<(), StartError> {
        self.stop_ws();
        // Set before the transport accepts anything: the API thread takes it before it
        // reads any input that arrives later.
        self.api.set_allow_remote_admin(allow_remote_admin);
        let addresses = ws.addresses.clone();
        let mut transport = WsTransport::new(ws);
        if let Err(e) = transport.start(self.api.inbox()) {
            // The transport's error is text; the port being taken is the one case the
            // user can fix, so it is told apart by trying the addresses again.
            let taken = addresses.iter().find(|a| {
                TcpListener::bind(a).is_err_and(|e| e.kind() == std::io::ErrorKind::AddrInUse)
            });
            return Err(match taken {
                Some(a) => StartError::PortInUse(a.port()),
                None => StartError::Other(e.to_string()),
            });
        }
        self.ws = Some(transport);
        Ok(())
    }

    /// Starts the WebSocket transport for network access at `port` (see
    /// [`RemoteServer::start_network`]).
    pub fn start_ws_network(
        &mut self,
        port: u16,
        network: NetworkOptions,
    ) -> Result<(), StartError> {
        let mut ws = WsConfig::network(port, network.identity);
        ws.policy.host_names = network.host_names;
        self.start_ws(ws, network.allow_remote_admin)
    }

    /// Starts the WebSocket transport on `127.0.0.1` and `::1` at `port`.
    pub fn start_ws_loopback(&mut self, port: u16) -> Result<(), StartError> {
        self.start_ws(WsConfig::loopback(port), false)
    }

    /// Stops the WebSocket transport, if it runs: its apps are disconnected, IPC
    /// sessions are not.
    pub fn stop_ws(&mut self) {
        if let Some(mut t) = self.ws.take() {
            t.stop();
        }
    }

    /// True while the WebSocket transport runs.
    pub fn serves_ws(&self) -> bool {
        self.ws.is_some()
    }

    /// Serves local IPC at `endpoint` as well, if it is not served yet. On failure
    /// (another RBS holds the endpoint, its directory cannot be used) nothing changes
    /// and the reason is returned; `ipc_problem` is left to the caller.
    pub fn start_ipc(&mut self, endpoint: Endpoint) -> Result<(), String> {
        if self.ipc.is_some() {
            return Ok(());
        }
        let mut ipc = IpcTransport::new(IpcConfig::new(endpoint));
        ipc.start(self.api.inbox()).map_err(|e| e.to_string())?;
        self.ipc = Some(ipc);
        self.ipc_problem = None;
        Ok(())
    }

    /// Stops the IPC transport, if it runs: its sessions are closed and the endpoint
    /// is removed (the socket file; on Windows the pipe is closed).
    pub fn stop_ipc(&mut self) {
        if let Some(mut t) = self.ipc.take() {
            t.stop();
        }
        self.ipc_problem = None;
    }

    /// Listens on `127.0.0.1` and `::1` at `port`.
    pub fn start_on_port(
        port: u16,
        tokens: TokenStore,
        options: RemoteOptions,
    ) -> Result<(RemoteServer, ControlEndpoint), StartError> {
        Self::start(WsConfig::loopback(port).addresses, tokens, options)
    }

    /// The tokens changed: new ones work from now on, and apps using a removed one are
    /// disconnected.
    pub fn set_tokens(&self, tokens: TokenStore) {
        self.api.set_tokens(tokens);
    }

    /// Replaces the token store and the AI tools flags (ADR-0134 decision 2).
    pub fn set_slot(&self, slot: TokenSlot) {
        self.api.set_slot(slot);
    }

    /// What the control thread needs to answer `tokens.*` on the settings in `config`.
    /// The store it sets is the union of both settings files, with `flags` as they are
    /// then, so a token change never stops AI tools.
    pub fn api_tokens(&self, config: PathBuf, flags: Arc<SlotFlags>) -> ApiTokens {
        ApiTokens::new(config, self.api.token_handle(), flags)
    }

    /// The local IPC endpoint it serves, if it does.
    pub fn ipc_endpoint(&self) -> Option<&Endpoint> {
        self.ipc.as_ref().map(IpcTransport::endpoint)
    }

    /// Why local IPC is not served although asked for (another RBS serves it, or the
    /// endpoint's directory cannot be used), or `None`.
    pub fn ipc_problem(&self) -> Option<&str> {
        self.ipc_problem.as_deref()
    }

    /// The addresses the WebSocket transport listens on (none while it is stopped).
    pub fn local_addrs(&self) -> &[SocketAddr] {
        self.ws.as_ref().map_or(&[], WsTransport::local_addrs)
    }

    /// Rounds the API thread has run (idle-cost check).
    pub fn rounds(&self) -> u64 {
        self.api.rounds()
    }
}
