//! Remote control while it is on: the control API's thread (`rbs_api::ApiServer`) and
//! its WebSocket transport on loopback (`rbs_api_ws`), wired together here because the
//! app is a composition root (P1, ADR-0044 decision 4). The caller attaches the returned
//! endpoint to the studio's control thread (`StudioHost::attach_api`).
//!
//! Nothing here exists while remote control is off (ADR-0013): the settings hold no
//! server, so no thread, socket or event buffer. On with no app connected, the API
//! thread and the transport's I/O thread sleep until a connection arrives.

use std::net::SocketAddr;

use rbs_api::request::ClientInfo;
use rbs_api::{
    ApiOptions, ApiServer, AuditEvent, ControlEndpoint, ControlTransport, ServerConfig, TokenStore,
};
use rbs_api_ws::{WsConfig, WsTransport};

/// A running remote control server. Dropping it stops the transport (connected apps
/// are told the server is going away), then the API thread.
pub struct RemoteServer {
    // Field order is drop order: the transport stops before the API thread.
    transport: WsTransport,
    api: ApiServer,
}

/// What the server needs from the app.
pub struct RemoteOptions {
    /// Wakes the studio's control thread when a request waits (`StudioHost::api_waker`).
    pub control_waker: Box<dyn Fn() + Send + Sync>,
    /// Logs connections, authentication and privileged calls (never a token).
    pub audit: Box<dyn FnMut(AuditEvent) + Send>,
}

impl RemoteServer {
    /// Starts the API thread and listens on `addresses`, which must be loopback. The
    /// returned endpoint goes to the control thread. On failure nothing keeps running.
    pub fn start(
        addresses: Vec<SocketAddr>,
        tokens: TokenStore,
        options: RemoteOptions,
    ) -> Result<(RemoteServer, ControlEndpoint), String> {
        let config = ServerConfig::new(ClientInfo::new("RBS", env!("CARGO_PKG_VERSION")))
            .map_err(|e| format!("the system could not make a random server id ({e})"))?;
        let mut api_options = ApiOptions::new(options.control_waker);
        api_options.audit = options.audit;
        let (api, endpoint) =
            ApiServer::start(config, tokens, api_options).map_err(|e| e.to_string())?;
        // Started here, not by `ApiServer::start`, so the bound addresses can be read.
        let mut transport = WsTransport::new(WsConfig::on(addresses));
        transport.start(api.inbox()).map_err(|e| e.to_string())?;
        Ok((RemoteServer { transport, api }, endpoint))
    }

    /// Listens on `127.0.0.1` and `::1` at `port`.
    pub fn start_on_port(
        port: u16,
        tokens: TokenStore,
        options: RemoteOptions,
    ) -> Result<(RemoteServer, ControlEndpoint), String> {
        Self::start(WsConfig::loopback(port).addresses, tokens, options)
    }

    /// The tokens changed: new ones work from now on, and apps using a removed one are
    /// disconnected.
    pub fn set_tokens(&self, tokens: TokenStore) {
        self.api.set_tokens(tokens);
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
