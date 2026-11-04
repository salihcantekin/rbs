//! Remote control while it is on: the control API's thread (`rbs_api::ApiServer`), its
//! WebSocket transport on loopback (`rbs_api_ws`) and its local IPC transport
//! (`rbs_api_ipc`, for `rbs-cli` and other programs of the same user), wired together
//! here because the app is a composition root (P1, ADR-0044 decision 4). The caller
//! attaches the returned endpoint to the studio's control thread
//! (`StudioHost::attach_api`), with [`ApiTokens`] so it answers `tokens.*`.
//!
//! Nothing here exists while remote control is off (ADR-0013): the settings hold no
//! server, so no thread, socket or event buffer. On with no app connected, the API
//! thread and the transport's I/O thread sleep until a connection arrives.

use std::fmt;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;

use rbs_api::request::{ClientInfo, Response};
use rbs_api::{
    ApiError, ApiOptions, ApiServer, AuditEvent, Command, ControlEndpoint, ControlTransport,
    ServerConfig, TokenHandle, TokenStore,
};
use rbs_api_ipc::{Endpoint, IpcConfig, IpcTransport};
use rbs_api_ws::{TlsIdentity, WsConfig, WsTransport};

/// A running remote control server. Dropping it stops the transport (connected apps
/// are told the server is going away), then the API thread.
pub struct RemoteServer {
    // Field order is drop order: the transports stop before the API thread.
    transport: WsTransport,
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
        options: RemoteOptions,
    ) -> Result<(RemoteServer, ControlEndpoint), StartError> {
        let addresses = ws.addresses.clone();
        let mut config = ServerConfig::new(ClientInfo::new("RBS", env!("CARGO_PKG_VERSION")))
            .map_err(|e| {
                StartError::Other(format!(
                    "the system could not make a random server id ({e})"
                ))
            })?;
        config.allow_remote_admin = allow_remote_admin;
        let mut api_options = ApiOptions::new(options.control_waker);
        api_options.audit = options.audit;
        let (api, endpoint) = ApiServer::start(config, tokens, api_options)
            .map_err(|e| StartError::Other(e.to_string()))?;
        // Started here, not by `ApiServer::start`, so the bound addresses can be read.
        let mut transport = WsTransport::new(ws);
        if let Err(e) = transport.start(api.inbox()) {
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
        let (ipc, ipc_problem) = match options.ipc {
            None => (None, None),
            Some(e) => {
                let mut ipc = IpcTransport::new(IpcConfig::new(e));
                match ipc.start(api.inbox()) {
                    Ok(()) => (Some(ipc), None),
                    Err(e) => (None, Some(e.to_string())),
                }
            }
        };
        Ok((
            RemoteServer {
                transport,
                ipc,
                ipc_problem,
                api,
            },
            endpoint,
        ))
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

    /// What the control thread needs to answer `tokens.*` on the settings in `config`.
    pub fn api_tokens(&self, config: PathBuf) -> ApiTokens {
        ApiTokens {
            config,
            handle: self.api.token_handle(),
        }
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

    /// The addresses it listens on.
    pub fn local_addrs(&self) -> &[SocketAddr] {
        self.transport.local_addrs()
    }

    /// Rounds the API thread has run (idle-cost check).
    pub fn rounds(&self) -> u64 {
        self.api.rounds()
    }
}

/// The control API's `tokens.*` methods for the control thread: the settings in
/// `config` (`rbs_remote_settings::token_command`) and the running server's tokens.
#[derive(Debug, Clone)]
pub struct ApiTokens {
    config: PathBuf,
    handle: TokenHandle,
}

impl ApiTokens {
    /// Answers `command` if it is a `tokens.*` command, `None` otherwise. A change is
    /// saved and the server takes the new tokens (under the settings lock) before this
    /// returns, so a new token works, or a revoked token's apps are disconnected,
    /// before the reply is sent. `true` with the reply when the tokens changed.
    pub fn answer(&self, command: &Command) -> Option<Result<(Response, bool), ApiError>> {
        let answer =
            rbs_remote_settings::token_command(&self.config, command, |s| self.handle.set(s))?;
        Some(answer.map(|a| (a.response, a.changed)))
    }
}
